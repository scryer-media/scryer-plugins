//! Transmission RPC download client.
//!
//! Reconciled against Sonarr's `Download/Clients/Transmission/*` and
//! `Download/Clients/Vuze/Vuze.cs`, but built on Scryer's contract rather than
//! transliterated from it: the core owns removal, seeding policy, remote-path
//! mapping and the post-import handoff, so this plugin's job is to observe
//! Transmission honestly (tri-state `can_remove`, `is_private`, the richer
//! `DownloadItemState`s, `completed_at`, transfer rates) and to execute what
//! the core routes to it.

use std::fmt;

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
const VERSION_VAR_KEY: &str = "transmission.client_version";
/// Cached when `session-get` answers without a `version`, so an unreadable
/// version still costs one round trip per instance rather than one per call.
const UNKNOWN_VERSION: &str = "unknown";

/// Sonarr's supported floor (`Transmission.cs:84`).
const MINIMUM_SUPPORTED_VERSION: ClientVersion = ClientVersion::new(2, 40);
/// `labels` on `torrent-add`/`torrent-set` arrived with RPC 17 in Transmission
/// 4.0, which is why Sonarr gates every label use on it
/// (`Transmission.cs:21`, `TransmissionBase.cs:51`).
const LABEL_SUPPORT_VERSION: ClientVersion = ClientVersion::new(4, 0);
/// Vuze/Azureus answer `session-get` with an RPC protocol version instead of a
/// Transmission client version (`Vuze.cs:16`, `Vuze.cs:58-72`).
const VUZE_MINIMUM_PROTOCOL_VERSION: u64 = 14;

/// `TimeSpan.FromSeconds` overflows past this many seconds, which is how Sonarr
/// notices an `eta` that is really milliseconds (`TransmissionBase.cs:91-101`,
/// fixture `should_support_long_values_for_eta_in_milliseconds`).
const MAX_ETA_SECONDS: i64 = 922_337_203_685;

const STATUS_STOPPED: i64 = 0;
const STATUS_CHECK_WAIT: i64 = 1;
const STATUS_CHECK: i64 = 2;
const STATUS_QUEUED: i64 = 3;
const STATUS_DOWNLOADING: i64 = 4;
const STATUS_SEEDING_WAIT: i64 = 5;
const STATUS_SEEDING: i64 = 6;

macro_rules! warn_log {
    ($($argument:tt)*) => {
        scryer_plugin_pdk::log::log(
            scryer_plugin_pdk::log::LogLevel::Warn,
            &format!($($argument)*),
        )
    };
}

#[derive(Debug, Clone)]
struct TransmissionConfig {
    rpc_url: String,
    username: String,
    password: String,
    category: String,
    imported_category: String,
    label_after_import: bool,
    recent_priority: PluginTorrentQueuePlacement,
    older_priority: PluginTorrentQueuePlacement,
    directory: String,
    add_paused: bool,
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
    #[serde(default, rename = "rateDownload")]
    rate_download: Option<i64>,
    #[serde(default, rename = "rateUpload")]
    rate_upload: Option<i64>,
    /// Unix seconds; `0` when the torrent has never finished.
    #[serde(default, rename = "doneDate")]
    done_date: i64,
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

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// `Err(Error::msg(..))` reaches the host as `PluginErrorCode::Temporary`, so
/// every failure this plugin can name carries its own code instead
/// (`00-common.md` rule 4, mirroring Sonarr's exception classes in
/// `TransmissionProxy.cs:263-374` and `TransmissionBase.cs:282-311`).
fn plugin_error(code: PluginErrorCode, public_message: impl Into<String>) -> PluginError {
    PluginError {
        code,
        public_message: public_message.into(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    }
}

fn detailed_error(
    code: PluginErrorCode,
    public_message: impl Into<String>,
    debug_message: impl Into<String>,
) -> PluginError {
    PluginError {
        debug_message: Some(debug_message.into()),
        ..plugin_error(code, public_message)
    }
}

fn respond<T: serde::Serialize>(result: Result<T, PluginError>) -> FnResult<String> {
    let result = match result {
        Ok(value) => PluginResult::Ok(value),
        Err(error) => PluginResult::Err(error),
    };
    Ok(serde_json::to_string(&result)?)
}

fn parse_request<T: serde::de::DeserializeOwned>(input: &str) -> Result<T, PluginError> {
    serde_json::from_str(input).map_err(|error| {
        detailed_error(
            PluginErrorCode::Permanent,
            "Scryer sent a request this plugin could not read.",
            error.to_string(),
        )
    })
}

// ---------------------------------------------------------------------------
// Client version
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientVersion {
    parts: [u64; 4],
    components: usize,
}

impl ClientVersion {
    const fn new(major: u64, minor: u64) -> Self {
        Self {
            parts: [major, minor, 0, 0],
            components: 2,
        }
    }

    fn at_least(self, floor: Self) -> bool {
        self.parts >= floor.parts
    }
}

impl fmt::Display for ClientVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, part) in self.parts[..self.components].iter().enumerate() {
            if index > 0 {
                formatter.write_str(".")?;
            }
            write!(formatter, "{part}")?;
        }
        Ok(())
    }
}

/// Sonarr reads the version with `(?<!\(|(\d|\.)+)(\d|\.)+(?!\)|(\d|\.)+)`
/// (`Transmission.cs:81`, `TransmissionBase.cs:334`): the first maximal run of
/// digits and dots that is neither opened by `(` nor closed by `)`. That is
/// what makes `2.84 (2.84)` report 2.84 and keeps `2.84+ ()` from tripping over
/// its own suffix.
fn parse_client_version(raw: &str) -> Option<ClientVersion> {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !is_version_byte(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_version_byte(bytes[index]) {
            index += 1;
        }
        let opened_by_parenthesis = start > 0 && bytes[start - 1] == b'(';
        let closed_by_parenthesis = bytes.get(index) == Some(&b')');
        if opened_by_parenthesis || closed_by_parenthesis {
            continue;
        }
        if let Some(version) = parse_version_components(&raw[start..index]) {
            return Some(version);
        }
    }
    None
}

fn is_version_byte(byte: u8) -> bool {
    byte.is_ascii_digit() || byte == b'.'
}

/// `Version.Parse` needs at least `major.minor` and at most four components,
/// and rejects an empty component — so `2.` and `3` are not versions.
fn parse_version_components(raw: &str) -> Option<ClientVersion> {
    let mut parts = [0_u64; 4];
    let mut components = 0;
    for piece in raw.split('.') {
        if components == parts.len() || piece.is_empty() {
            return None;
        }
        parts[components] = piece.parse().ok()?;
        components += 1;
    }
    (components >= 2).then_some(ClientVersion { parts, components })
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

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
                    // Transmission has no total-seed-time limit; the goal is
                    // mapped onto `seedIdleLimit`, exactly as Sonarr does
                    // (`TransmissionProxy.cs:117-121`). Documented in README.
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
                // The post-import mark the core actually calls. Both marks run
                // the same label-only body; neither removes anything.
                mark_imported_non_destructive: true,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

pub fn scryer_download_add(input: String) -> FnResult<String> {
    respond(add(&input))
}

fn add(input: &str) -> Result<PluginDownloadClientAddResponse, PluginError> {
    let request: PluginDownloadClientAddRequest = parse_request(input)?;
    let config = TransmissionConfig::from_host();
    if let Some(problem) = conflicting_settings(&config) {
        return Err(plugin_error(PluginErrorCode::InvalidConfig, problem));
    }

    let mut arguments = serde_json::Map::new();
    if let Some(torrent_bytes_base64) = request.source.torrent_bytes_base64.as_deref() {
        arguments.insert(
            "metainfo".to_string(),
            serde_json::Value::String(torrent_bytes_base64.to_string()),
        );
    } else if let Some(source) = source_url(&request) {
        arguments.insert("filename".to_string(), serde_json::Value::String(source));
    } else {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "download source is missing",
        ));
    }

    arguments.insert(
        "paused".to_string(),
        serde_json::Value::Bool(request_paused(&config, &request)),
    );
    // Resolved before the version probe on purpose: when this needs the
    // session it also warms the version cache, so an add costs one
    // `session-get`, not two.
    if let Some(download_dir) = download_directory(&config, &request)? {
        arguments.insert(
            "download-dir".to_string(),
            serde_json::Value::String(download_dir),
        );
    }
    // Transmission 3.x ignores an unknown `torrent-add` argument silently, so
    // sending `labels` to it is not an error — it is worse than that, because
    // the queue would then be scoped against labels no torrent will ever
    // carry. Gate it the way Sonarr does (`Transmission.cs:21`).
    if supports_labels(&config)? {
        let labels = labels_for_request(&config, &request);
        if !labels.is_empty() {
            arguments.insert(
                "labels".to_string(),
                serde_json::to_value(labels).map_err(encode_error)?,
            );
        }
    }

    let response = rpc(
        &config,
        "torrent-add",
        Some(serde_json::Value::Object(arguments)),
    )?;
    // Sonarr does not distinguish `torrent-duplicate` from `torrent-added`
    // either (`TransmissionProxy.cs:57-97`): a torrent already in the client is
    // the outcome the grab wanted.
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
        .ok_or_else(|| {
            plugin_error(
                PluginErrorCode::Permanent,
                "Transmission accepted the torrent but reported no hash, and the release carried none either.",
            )
        })?;

    apply_seed_limits(&config, &hash, &request)?;
    if should_move_to_top(&config, &request) {
        // The torrent is already in the client; a failed queue move is a
        // placement miss, not a failed grab.
        if let Err(error) = rpc(
            &config,
            "queue-move-top",
            Some(serde_json::json!({ "ids": [hash.clone()] })),
        ) {
            warn_log!(
                "Transmission accepted {hash} but refused queue-move-top: {}",
                error.public_message
            );
        }
    }

    Ok(PluginDownloadClientAddResponse {
        client_item_id: hash.clone(),
        info_hash: Some(hash),
    })
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
    respond(list_queue())
}

fn list_queue() -> Result<Vec<PluginDownloadItem>, PluginError> {
    let config = TransmissionConfig::from_host();
    let session = session_get(&config)?;
    let supports_labels = session_supports_labels(&session);
    let torrents = list_torrents(&config)?;
    Ok(torrents
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent, supports_labels))
        .map(|torrent| torrent_to_item(&config, &session, torrent))
        .collect())
}

/// Transmission keeps no failed-download history: a torrent that errors stays
/// in the same `torrent-get` listing with an `errorString`, and `list_queue`
/// already reports it as `Warning`. The bridge merges this into the queue poll
/// keeping only `Failed`/`Error` rows
/// (`pdk/scryer-plugin-pdk/src/download_client_bridge.rs:166-203`), so running
/// a second full `torrent-get` here can only ever contribute nothing. One RPC
/// per poll instead of two.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    respond(Ok::<Vec<PluginDownloadItem>, PluginError>(Vec::new()))
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    respond(list_completed())
}

fn list_completed() -> Result<Vec<PluginCompletedDownload>, PluginError> {
    let config = TransmissionConfig::from_host();
    let supports_labels = supports_labels(&config)?;
    Ok(list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent, supports_labels))
        .filter(is_completed)
        .map(|torrent| torrent_to_completed(&config, torrent))
        .collect())
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    respond(control(&input))
}

fn control(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientControlRequest = parse_request(input)?;
    let config = TransmissionConfig::from_host();
    let hash = normalize_hash(&request.client_item_id);
    if hash.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ));
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
            return Err(plugin_error(
                PluginErrorCode::Unsupported,
                "Transmission does not support force_start through this plugin",
            ));
        }
    }

    Ok(())
}

/// The destructive mark has no core caller, and removing a torrent at import
/// time is precisely what Scryer's seeding gate exists to prevent — so it runs
/// the same non-destructive body rather than a second, riskier one.
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    respond(mark_imported(&input))
}

pub fn scryer_download_mark_imported_non_destructive(input: String) -> FnResult<String> {
    respond(mark_imported(&input))
}

/// Sonarr's `Transmission.MarkItemAsImported` (`Transmission.cs:36-73`) in
/// Scryer's shape: swap the scope label for the post-import label, never
/// remove, and treat a torrent that is gone as a warning rather than a failure.
fn mark_imported(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientMarkImportedRequest = parse_request(input)?;
    let config = TransmissionConfig::from_host();
    let hash = normalize_hash(
        request
            .info_hash
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(request.client_item_id.as_str()),
    );
    if hash.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ));
    }

    if !config.label_after_import {
        return Ok(());
    }
    let imported_label = config.imported_category.trim().to_string();
    if imported_label.is_empty() {
        return Ok(());
    }
    if !supports_labels(&config)? {
        warn_log!(
            "Transmission below 4.0 has no labels; skipping the post-import label \"{imported_label}\"."
        );
        return Ok(());
    }

    let Some(existing) = torrent_labels(&config, &hash)? else {
        warn_log!("Could not find torrent with hash \"{hash}\" in Transmission.");
        return Ok(());
    };

    let scope_label = post_import_scope_label(&config, &request);
    let labels = swap_labels(&existing, scope_label.as_deref(), &imported_label);
    if same_label_set(&existing, &labels) {
        return Ok(());
    }

    rpc(
        &config,
        "torrent-set",
        Some(serde_json::json!({ "ids": [hash], "labels": labels })),
    )?;
    Ok(())
}

/// The label that scoped this download to Scryer, and therefore the one the
/// post-import label replaces.
///
/// The core populates `post_import_isolation` from the *download's* category
/// (`crates/scryer-plugins/src/download_client_adapter.rs:657-674`), which is
/// the routed scope label, not a new one — so it is what gets dropped, with the
/// tracked category and finally the configured category as fallbacks.
fn post_import_scope_label(
    config: &TransmissionConfig,
    request: &PluginDownloadClientMarkImportedRequest,
) -> Option<String> {
    request
        .post_import_isolation
        .iter()
        .find(|entry| {
            matches!(
                entry.mode,
                DownloadIsolationMode::Tag
                    | DownloadIsolationMode::Label
                    | DownloadIsolationMode::Category
            )
        })
        .map(|entry| entry.value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| trimmed_non_empty(request.category.as_deref().unwrap_or_default()))
        .or_else(|| trimmed_non_empty(&config.category))
}

/// Sonarr builds a case-insensitive set, adds the imported label and removes
/// the category, guarded by `TvImportedCategory != TvCategory`
/// (`Transmission.cs:44-65`). Existing labels keep the client's own casing.
fn swap_labels(existing: &[String], scope: Option<&str>, imported: &str) -> Vec<String> {
    let mut labels: Vec<String> = existing
        .iter()
        .filter(|label| {
            !scope.is_some_and(|scope| {
                label.eq_ignore_ascii_case(scope) && !scope.eq_ignore_ascii_case(imported)
            })
        })
        .cloned()
        .collect();
    if !labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(imported))
    {
        labels.push(imported.to_string());
    }
    labels
}

fn same_label_set(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|label| right.iter().any(|other| other.eq_ignore_ascii_case(label)))
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    respond(status())
}

fn status() -> Result<PluginDownloadClientStatus, PluginError> {
    let config = TransmissionConfig::from_host();
    let session = session_get(&config)?;
    let roots = effective_output_root(&config, &session)
        .into_iter()
        .collect();

    let mut warnings = Vec::new();
    if let Some(problem) = settings_problem(&config) {
        warnings.push(problem);
    }
    if !session_supports_labels(&session)
        && (!config.category.is_empty() || !config.imported_category.is_empty())
    {
        warnings.push(
            "This Transmission is older than 4.0 and has no labels: the category is applied as a download sub-directory and the post-import label is skipped."
                .to_string(),
        );
    }

    Ok(PluginDownloadClientStatus {
        version: session.version.or(session.rpc_version),
        is_localhost: Some(is_localhost_url(&config.rpc_url)),
        remote_output_roots: roots,
        // Removal is the core's decision through the seeding gate; this plugin
        // never removes a finished torrent on its own.
        removes_completed_downloads: Some(false),
        sorting_mode: Some("transmission-rpc".to_string()),
        warnings,
    })
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    respond(test_connection())
}

/// Sonarr's `Test` is `TestConnection()` then `TestGetTorrents()`
/// (`TransmissionBase.cs:248-257`), with `ValidateVersion` inside the first
/// (`Transmission.cs:75-90`). Settings validation runs first here because
/// Scryer has no separate settings validator to run it.
fn test_connection() -> Result<String, PluginError> {
    let config = TransmissionConfig::from_host();
    if let Some(problem) = settings_problem(&config) {
        return Err(plugin_error(PluginErrorCode::InvalidConfig, problem));
    }

    // `GetClientVersion(settings, force: true)` (`TransmissionProxy.cs:139-149`):
    // a test must never pass on a cached answer.
    forget_var(SESSION_VAR_KEY);
    forget_var(VERSION_VAR_KEY);

    let session = session_get(&config)?;
    let reported = session
        .version
        .clone()
        .or_else(|| session.rpc_version.clone())
        .unwrap_or_default();

    match session.version.as_deref().and_then(parse_client_version) {
        Some(version) if version.at_least(MINIMUM_SUPPORTED_VERSION) => {
            test_get_torrents(&config)?;
            Ok(version.to_string())
        }
        Some(version) => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "Transmission {version} is not supported; version {MINIMUM_SUPPORTED_VERSION} or newer is required."
            ),
        )),
        None => {
            // Vuze/Azureus reach this plugin through its provider aliases and
            // answer with an RPC protocol version instead of a Transmission
            // version; Sonarr validates them on protocol 14+ (`Vuze.cs:58-72`).
            let protocol = session
                .rpc_version
                .as_deref()
                .and_then(|value| value.trim().parse::<u64>().ok());
            match protocol {
                Some(protocol) if protocol >= VUZE_MINIMUM_PROTOCOL_VERSION => {
                    test_get_torrents(&config)?;
                    Ok(format!("RPC {protocol}"))
                }
                _ => Err(plugin_error(
                    PluginErrorCode::InvalidConfig,
                    format!(
                        "Transmission reported the version \"{reported}\", which is neither a client version {MINIMUM_SUPPORTED_VERSION} or newer nor an RPC protocol version {VUZE_MINIMUM_PROTOCOL_VERSION} or newer."
                    ),
                )),
            }
        }
    }
}

fn test_get_torrents(config: &TransmissionConfig) -> Result<(), PluginError> {
    list_torrents(config)
        .map(|_| ())
        .map_err(|error| PluginError {
            public_message: format!("Failed to get torrents: {}", error.public_message),
            ..error
        })
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

impl TransmissionConfig {
    fn from_host() -> Self {
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

        Self {
            rpc_url,
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            category,
            imported_category: config_value("post_import_category").unwrap_or_default(),
            label_after_import: resolve_label_after_import(
                config_value("label_after_import").as_deref(),
                config_value("post_import_action").as_deref(),
            ),
            recent_priority: queue_placement_config("recent_priority"),
            older_priority: queue_placement_config("older_priority"),
            directory: config_value("directory").unwrap_or_default(),
            add_paused: config_bool("add_paused", false),
        }
    }
}

/// `post_import_action` is retired: `remove`/`remove_with_data` asked this
/// plugin to delete a seeding torrent at import time, which the core's seeding
/// gate exists to forbid, and the destructive mark it hung off has no caller.
/// The key is still read so an existing configuration keeps parsing — `retain`
/// means "leave the labels alone", anything else means "apply the label".
fn resolve_label_after_import(explicit: Option<&str>, legacy_action: Option<&str>) -> bool {
    explicit.map(config_bool_value).unwrap_or_else(|| {
        !legacy_action.is_some_and(|action| action.trim().eq_ignore_ascii_case("retain"))
    })
}

/// Sonarr's `TransmissionSettingsValidator` (`TransmissionSettings.cs:10-24`).
/// Scryer has no settings validator, so the same two rules surface through
/// `test_connection` and the client status warnings.
fn settings_problem(config: &TransmissionConfig) -> Option<String> {
    if let Some(problem) = conflicting_settings(config) {
        return Some(problem);
    }
    (!config.category.is_empty() && !is_valid_category(&config.category)).then(|| {
        format!(
            "Category \"{}\" is invalid: allowed characters are a-z and -, with an optional leading dot.",
            config.category
        )
    })
}

/// The half of the validator that makes an *add* incoherent rather than merely
/// unconventional: with both set, the torrent is forced into `directory` while
/// the queue is scoped by label, so the two settings describe different sets.
fn conflicting_settings(config: &TransmissionConfig) -> Option<String> {
    (!config.category.is_empty() && !config.directory.is_empty())
        .then(|| "Cannot use Category and Directory together: clear one of them.".to_string())
}

/// Sonarr: `^\.?[-a-z]*$`, case-insensitive.
fn is_valid_category(value: &str) -> bool {
    value
        .strip_prefix('.')
        .unwrap_or(value)
        .chars()
        .all(|character| character == '-' || character.is_ascii_alphabetic())
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
            Some(
                "Transmission label Scryer scopes its queue to (Transmission 4.0+); on older releases it is used as a download sub-directory instead. Allowed characters a-z and -.",
            ),
        ),
        field(
            "post_import_category",
            "Post Import Category",
            ConfigFieldType::String,
            false,
            None,
            Some("Label applied after Scryer imports the download, replacing the category label"),
        ),
        field(
            "label_after_import",
            "Label After Import",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            Some(
                "Apply the post-import label once Scryer has imported the download. Scryer never removes a torrent here; removal stays with the seeding policy.",
            ),
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
            Some("Optional download directory. Cannot be combined with Category."),
        ),
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

// ---------------------------------------------------------------------------
// RPC transport
// ---------------------------------------------------------------------------

fn rpc(
    config: &TransmissionConfig,
    method: &str,
    arguments: Option<serde_json::Value>,
) -> Result<RpcResponse, PluginError> {
    let body = match arguments {
        Some(arguments) => serde_json::json!({ "method": method, "arguments": arguments }),
        None => serde_json::json!({ "method": method }),
    };
    let encoded = serde_json::to_vec(&body).map_err(encode_error)?;

    let mut response = rpc_once(config, &encoded, cached_var(SESSION_VAR_KEY))?;
    if response.status_code() == 409 {
        let session_id = extract_session_id(&response).ok_or_else(|| {
            plugin_error(
                PluginErrorCode::InvalidConfig,
                "Remote host did not return a Session Id; check that the URL base points at Transmission's RPC endpoint.",
            )
        })?;
        remember_var(SESSION_VAR_KEY, &session_id);
        response = rpc_once(config, &encoded, Some(session_id))?;
    }

    let status = response.status_code();
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    if let Some(error) = classify_http_status(
        status,
        header_value(&response, "location").as_deref(),
        &body_text,
    ) {
        return Err(error);
    }

    let parsed: RpcResponse = serde_json::from_str(&body_text).map_err(|error| {
        detailed_error(
            PluginErrorCode::InvalidConfig,
            "The configured URL did not answer with a Transmission RPC response; check host, port and URL base.",
            format!("{error}: {}", truncate(&body_text)),
        )
    })?;
    if parsed.result != "success" {
        return Err(detailed_error(
            PluginErrorCode::Permanent,
            format!("Transmission rejected {method}: {}", parsed.result),
            truncate(&body_text),
        ));
    }
    Ok(parsed)
}

/// The host runs plugin HTTP with `redirect::Policy::none()`
/// (`scryer-outbound-http::prepare_plugin_blocking_http_target`), which is
/// Sonarr's `AllowAutoRedirect = false` (`TransmissionProxy.cs:258`): a login
/// page behind a redirect arrives here as a 3xx, not as a JSON parse failure.
fn classify_http_status(status: u16, location: Option<&str>, body: &str) -> Option<PluginError> {
    match status {
        200..=299 => None,
        300..=399 => Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            match location.map(str::trim).filter(|value| !value.is_empty()) {
                Some(location) => format!("Remote site redirected to {location}"),
                None => "Remote site redirected the RPC request; check host, port and URL base."
                    .to_string(),
            },
        )),
        401 => Some(plugin_error(
            PluginErrorCode::AuthFailed,
            "Failed to authenticate with Transmission: user authentication failed.",
        )),
        403 => Some(plugin_error(
            PluginErrorCode::AuthFailed,
            "Failed to authenticate with Transmission. It may be necessary to add Scryer's IP address to the RPC whitelist.",
        )),
        404 => Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            "Transmission's RPC endpoint was not found; check the URL base.",
        )),
        409 => Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            "Transmission kept rejecting the session id; check the URL base and any reverse proxy in front of it.",
        )),
        429 => Some(PluginError {
            retry_after_seconds: Some(60),
            ..plugin_error(
                PluginErrorCode::Temporary,
                "Transmission is rate limiting Scryer.",
            )
        }),
        500..=599 => Some(detailed_error(
            PluginErrorCode::Temporary,
            format!("Transmission returned HTTP {status}."),
            truncate(body),
        )),
        _ => Some(detailed_error(
            PluginErrorCode::Permanent,
            format!("Transmission returned HTTP {status}."),
            truncate(body),
        )),
    }
}

fn rpc_once(
    config: &TransmissionConfig,
    body: &[u8],
    session_id: Option<String>,
) -> Result<HttpResponse, PluginError> {
    let mut request = HttpRequest::new(&config.rpc_url)
        .with_method("POST")
        .with_header("Accept", "application/json")
        .with_header("Content-Type", "application/json")
        .with_header(
            "User-Agent",
            concat!("scryer-transmission-plugin/", env!("CARGO_PKG_VERSION")),
        );
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
    http::request::<Vec<u8>>(&request, Some(body.to_vec()))
        .map_err(|error| classify_transport_error(&error.to_string()))
}

/// The host hands transport failures back as a string, so classification is by
/// substring. It reports a timeout as the literal `timeout`
/// (`crates/scryer-plugins/src/plugin_http_host.rs:911-916`); a TLS trust
/// failure is only distinguishable by the text reqwest produced, which is the
/// closest this surface gets to Sonarr's `WebExceptionStatus.TrustFailure`
/// (`TransmissionProxy.cs:365-372`).
fn classify_transport_error(detail: &str) -> PluginError {
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("timeout") || lowered.contains("timed out") {
        detailed_error(
            PluginErrorCode::Temporary,
            "Transmission did not answer in time.",
            detail,
        )
    } else if lowered.contains("certificate")
        || lowered.contains("tls")
        || lowered.contains("ssl")
        || lowered.contains("trust")
    {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Transmission: certificate validation failed.",
            detail,
        )
    } else {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Transmission, please check your settings.",
            detail,
        )
    }
}

fn encode_error(error: serde_json::Error) -> PluginError {
    detailed_error(
        PluginErrorCode::Permanent,
        "Failed to encode a Transmission RPC request.",
        error.to_string(),
    )
}

fn truncate(value: &str) -> String {
    const LIMIT: usize = 512;
    match value.char_indices().nth(LIMIT) {
        Some((index, _)) => format!("{}…", &value[..index]),
        None => value.to_string(),
    }
}

fn header_value(response: &HttpResponse, name: &str) -> Option<String> {
    response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn extract_session_id(response: &HttpResponse) -> Option<String> {
    header_value(response, "X-Transmission-Session-Id")
}

/// Plugin state is best-effort: a host that refuses it costs an extra 409
/// handshake per call, which is far better than failing every RPC.
fn cached_var(key: &str) -> Option<String> {
    var::get(key)
        .ok()
        .flatten()
        .map(|value: String| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn remember_var(key: &str, value: &str) {
    let _ = var::set(key, value.to_string());
}

fn forget_var(key: &str) {
    let _ = var::remove(key);
}

fn session_get(config: &TransmissionConfig) -> Result<SessionConfig, PluginError> {
    let response = rpc(config, "session-get", None)?;
    let session: SessionConfig = serde_json::from_value(response.arguments).map_err(|error| {
        detailed_error(
            PluginErrorCode::InvalidConfig,
            "Transmission returned a session payload this plugin could not read.",
            error.to_string(),
        )
    })?;
    remember_var(
        VERSION_VAR_KEY,
        session.version.as_deref().unwrap_or(UNKNOWN_VERSION),
    );
    Ok(session)
}

/// The parsed version for the instance's lifetime. Host state survives across
/// invocations but not across restarts, which matches Sonarr's six-hour cache
/// closely enough that the probe is paid once per client, not once per poll.
fn client_version(config: &TransmissionConfig) -> Result<Option<ClientVersion>, PluginError> {
    if let Some(cached) = cached_var(VERSION_VAR_KEY) {
        return Ok(parse_client_version(&cached));
    }
    Ok(session_get(config)?
        .version
        .as_deref()
        .and_then(parse_client_version))
}

fn supports_labels(config: &TransmissionConfig) -> Result<bool, PluginError> {
    Ok(client_version(config)?.is_some_and(|version| version.at_least(LABEL_SUPPORT_VERSION)))
}

fn session_supports_labels(session: &SessionConfig) -> bool {
    session
        .version
        .as_deref()
        .and_then(parse_client_version)
        .is_some_and(|version| version.at_least(LABEL_SUPPORT_VERSION))
}

fn list_torrents(config: &TransmissionConfig) -> Result<Vec<TransmissionTorrent>, PluginError> {
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
        "rateDownload",
        "rateUpload",
        "doneDate",
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
    parse_torrents(&response)
}

fn parse_torrents(response: &RpcResponse) -> Result<Vec<TransmissionTorrent>, PluginError> {
    serde_json::from_value(
        response
            .arguments
            .get("torrents")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
    )
    .map_err(|error| {
        detailed_error(
            PluginErrorCode::InvalidConfig,
            "Transmission returned a torrent listing this plugin could not read.",
            error.to_string(),
        )
    })
}

/// `Ok(None)` when Transmission no longer has the torrent — Sonarr warns and
/// returns in the same situation (`Transmission.cs:50-54`).
fn torrent_labels(
    config: &TransmissionConfig,
    hash: &str,
) -> Result<Option<Vec<String>>, PluginError> {
    let response = rpc(
        config,
        "torrent-get",
        Some(serde_json::json!({
            "fields": ["labels"],
            "ids": [hash],
        })),
    )?;
    Ok(parse_torrents(&response)?
        .into_iter()
        .next()
        .map(|torrent| torrent.labels))
}

// ---------------------------------------------------------------------------
// Add-time helpers
// ---------------------------------------------------------------------------

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
) -> Result<Option<String>, PluginError> {
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

/// Sonarr's `GetDownloadDirectory`/`GetStatus` root
/// (`TransmissionBase.cs:190-214`, `264-280`): the configured directory wins,
/// otherwise the session's `download-dir` with the category appended as a
/// sub-folder.
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

/// The configured category is the scope label the queue is matched on, so it
/// leads; the core's routed isolation value rides along when it differs, which
/// is what lets Scryer route a download to its own label without dropping it
/// out of scope.
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
        && !labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(value.trim()))
    {
        labels.push(value.trim().to_string());
    }
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
) -> Result<(), PluginError> {
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

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

fn torrent_to_item(
    config: &TransmissionConfig,
    session: &SessionConfig,
    torrent: TransmissionTorrent,
) -> PluginDownloadItem {
    let hash = normalize_hash(&torrent.hash_string);
    let state = map_state(&torrent);
    let completed = is_completed(&torrent);
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
    let can_remove = derive_can_remove(session, &torrent, completed, ratio);
    let message = trimmed_non_empty(&torrent.error_string);

    PluginDownloadItem {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash.clone()),
        title: torrent.name.clone(),
        state,
        message: message.clone(),
        category: reported_category(config, &torrent),
        remote_output_path: Some(remote_output_path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(hash),
            client_native_id: torrent.id.map(|id| id.to_string()),
            labels: torrent.labels.clone(),
            save_path: Some(torrent.download_dir.clone()),
            content_paths: vec![remote_output_path],
            uploaded_bytes: Some(torrent.uploaded_ever),
            downloaded_bytes: Some(torrent.downloaded_ever),
            upload_rate_bytes_per_second: torrent.rate_upload,
            download_rate_bytes_per_second: torrent.rate_download,
            seed_ratio: ratio,
            seed_time_seconds: Some(torrent.seconds_seeding),
            // `totalSize == 0` is Transmission still fetching a magnet's
            // metadata, which is exactly what this field is for.
            metadata_only: Some(torrent.total_size == 0),
            is_private: torrent.is_private,
            raw_status: Some(torrent.status.to_string()),
            status_reason: message,
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.total_size),
        remaining_size_bytes: Some(torrent.left_until_done),
        eta_seconds: normalized_eta(torrent.eta),
        progress_percent,
        // Data completeness only: whether moving is *safe* while seeding is a Scryer-side
        // policy decision that combines this with the resolved seeding goal.
        can_move_files: Some(completed),
        can_remove,
        removed: Some(false),
        raw_state: Some(torrent.status.to_string()),
        completed_at: unix_to_rfc3339(torrent.done_date),
    }
}

fn torrent_to_completed(
    config: &TransmissionConfig,
    torrent: TransmissionTorrent,
) -> PluginCompletedDownload {
    let hash = normalize_hash(&torrent.hash_string);
    let path = output_path(&torrent);
    PluginCompletedDownload {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash),
        name: torrent.name.clone(),
        dest_dir: path.clone(),
        category: reported_category(config, &torrent),
        output_kind: Some(match torrent_file_count(&torrent) {
            0 => PluginDownloadOutputKind::Unknown,
            1 => PluginDownloadOutputKind::File,
            _ => PluginDownloadOutputKind::Directory,
        }),
        content_paths: vec![path],
        size_bytes: Some(torrent.total_size),
        completed_at: unix_to_rfc3339(torrent.done_date),
        parameters: Vec::new(),
        release_name: None,
    }
}

/// Sonarr reports `Settings.TvCategory` verbatim (`TransmissionBase.cs:82`).
/// Scryer reports the client's own casing for the label that matched, and
/// nothing at all when no label did — a category the torrent does not carry is
/// not something this client observed
/// (`memory: download-category-case-sensitivity`).
fn reported_category(config: &TransmissionConfig, torrent: &TransmissionTorrent) -> Option<String> {
    if config.category.is_empty() {
        return None;
    }
    torrent
        .labels
        .iter()
        .find(|label| label.eq_ignore_ascii_case(&config.category))
        .cloned()
}

/// Sonarr falls back to milliseconds when `TimeSpan.FromSeconds` overflows
/// (`TransmissionBase.cs:91-101`); `-1` (unknown) and `-2` (magnet) stay unset.
fn normalized_eta(eta: i64) -> Option<i64> {
    match eta {
        eta if eta < 0 => None,
        eta if eta > MAX_ETA_SECONDS => Some(eta / 1_000),
        eta => Some(eta),
    }
}

fn unix_to_rfc3339(timestamp: i64) -> Option<String> {
    if timestamp <= 0 {
        return None;
    }
    let days = timestamp.div_euclid(86_400);
    let seconds_of_day = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = month_position + if month_position < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

/// Transmission puts every torrent in `downloadDir` under its own name;
/// `:` is not legal on every filesystem Transmission runs on, and Sonarr
/// substitutes it (`TransmissionBase.cs:259-262`).
fn output_path(torrent: &TransmissionTorrent) -> String {
    let directory = torrent.download_dir.trim_end_matches(['/', '\\']);
    if is_vuze_shaped(torrent) {
        return vuze_output_path(directory, torrent);
    }
    format!("{directory}/{}", torrent.name.replace(':', "_"))
}

/// Vuze reaches this plugin through its `vuze`/`azureus` provider aliases, and
/// it lays downloads out like uTorrent: a multi-file torrent's `downloadDir`
/// *is* the job folder, while a single-file torrent sits directly in the root
/// (`Vuze.cs:39-56`).
///
/// The discriminator is the payload itself: Vuze spells the field `fileCount`
/// where Transmission spells it `file-count`, which is why Sonarr's model
/// carries both (`TransmissionTorrent.cs:28-34`). A response that has only the
/// Vuze spelling did not come from Transmission.
fn vuze_output_path(directory: &str, torrent: &TransmissionTorrent) -> String {
    let last_segment = directory.rsplit(['/', '\\']).next().unwrap_or_default();
    if last_segment == torrent.name || torrent_file_count(torrent) > 1 {
        return directory.to_string();
    }
    format!("{directory}/{}", torrent.name)
}

fn is_vuze_shaped(torrent: &TransmissionTorrent) -> bool {
    torrent.vuze_file_count.is_some() && torrent.file_count.is_none()
}

fn torrent_file_count(torrent: &TransmissionTorrent) -> i64 {
    torrent
        .file_count
        .or(torrent.vuze_file_count)
        .unwrap_or_default()
}

/// Sonarr's table (`TransmissionBase.cs:103-134`) with Scryer's richer states
/// where they are strictly more informative: a stopped torrent that is not
/// finished is `Paused` rather than `Downloading`, verification is `Verifying`,
/// and a finished torrent Transmission is actively seeding is `Seeding` — which
/// the core maps to the same `Completed` queue state
/// (`crates/scryer-plugins/src/download_client_adapter.rs:332`).
fn map_state(torrent: &TransmissionTorrent) -> DownloadItemState {
    if !torrent.error_string.trim().is_empty() {
        return DownloadItemState::Warning;
    }
    if torrent.total_size == 0 {
        return DownloadItemState::Queued;
    }
    if is_completed(torrent) {
        return if torrent.status == STATUS_SEEDING {
            DownloadItemState::Seeding
        } else {
            DownloadItemState::Completed
        };
    }
    match torrent.status {
        STATUS_STOPPED => DownloadItemState::Paused,
        STATUS_CHECK_WAIT | STATUS_CHECK => DownloadItemState::Verifying,
        STATUS_QUEUED => DownloadItemState::Queued,
        // Seeding and SeedingWait with data still missing are Sonarr's final
        // `else { Downloading }`.
        STATUS_DOWNLOADING | STATUS_SEEDING_WAIT | STATUS_SEEDING => DownloadItemState::Downloading,
        // A status code outside 0..=6 is a newer Transmission, not a fault:
        // Transmission reports real faults through `errorString`, handled
        // above. Keep polling rather than parking the row in a state nothing
        // ever clears.
        _ => DownloadItemState::Downloading,
    }
}

fn is_completed(torrent: &TransmissionTorrent) -> bool {
    if torrent.total_size == 0 {
        return false;
    }
    (torrent.left_until_done == 0
        && matches!(
            torrent.status,
            STATUS_STOPPED | STATUS_SEEDING_WAIT | STATUS_SEEDING
        ))
        || (torrent.is_finished && !matches!(torrent.status, STATUS_CHECK_WAIT | STATUS_CHECK))
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
    let is_stopped = torrent.status == STATUS_STOPPED;
    let is_seeding = torrent.status == STATUS_SEEDING;
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
    completed: bool,
    ratio: Option<f64>,
) -> Option<bool> {
    if !completed {
        return Some(false);
    }
    match seed_limit_state(session, torrent, ratio) {
        SeedLimitState::Met if torrent.status == STATUS_STOPPED => Some(true),
        // Limit satisfied but Transmission has not stopped the torrent yet.
        SeedLimitState::Met => None,
        SeedLimitState::Unmet => Some(false),
        SeedLimitState::Unknown => None,
    }
}

/// Sonarr's scope filter (`TransmissionBase.cs:51-75`): labels only when the
/// client actually has them, otherwise the directory, otherwise the category as
/// a path component.
fn torrent_matches_scope(
    config: &TransmissionConfig,
    torrent: &TransmissionTorrent,
    supports_labels: bool,
) -> bool {
    if !config.category.is_empty() && supports_labels && !torrent.labels.is_empty() {
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

fn trimmed_non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn config_bool_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn config_bool(key: &str, default: bool) -> bool {
    config_value(key)
        .map(|value| config_bool_value(&value))
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

    fn base_config() -> TransmissionConfig {
        TransmissionConfig {
            rpc_url: "http://localhost:9091/transmission/rpc".to_string(),
            username: String::new(),
            password: String::new(),
            category: String::new(),
            imported_category: String::new(),
            label_after_import: true,
            recent_priority: PluginTorrentQueuePlacement::Last,
            older_priority: PluginTorrentQueuePlacement::Last,
            directory: String::new(),
            add_paused: false,
        }
    }

    /// Sonarr's `_queued`: nothing downloaded yet (`TransmissionFixtureBase.cs:39-48`).
    fn queued_torrent(status: i64) -> TransmissionTorrent {
        TransmissionTorrent {
            hash_string: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            name: "Title.S01E01".to_string(),
            download_dir: "somepath".to_string(),
            total_size: 1_000,
            left_until_done: 1_000,
            is_finished: false,
            status,
            ..TransmissionTorrent::default()
        }
    }

    /// Sonarr's `_downloading` (`TransmissionFixtureBase.cs:50-59`).
    fn downloading_torrent(status: i64) -> TransmissionTorrent {
        TransmissionTorrent {
            left_until_done: 100,
            ..queued_torrent(status)
        }
    }

    /// Sonarr's `_completed` (`TransmissionFixtureBase.cs:73-84`).
    fn complete_torrent(status: i64) -> TransmissionTorrent {
        TransmissionTorrent {
            total_size: 1_000,
            left_until_done: 0,
            is_finished: true,
            downloaded_ever: 1_000,
            uploaded_ever: 900,
            ..queued_torrent(status)
        }
    }

    fn item(session: &SessionConfig, torrent: TransmissionTorrent) -> PluginDownloadItem {
        torrent_to_item(&base_config(), session, torrent)
    }

    // -----------------------------------------------------------------------
    // Status table (TransmissionFixture.cs:159-208)
    // -----------------------------------------------------------------------

    #[test]
    fn an_incomplete_torrent_maps_sonarrs_queued_status_table() {
        // Sonarr collapses Stopped/Check/CheckWait into Downloading; Scryer has
        // `Paused` and `Verifying`, which are non-terminal and strictly more
        // informative, so they are used instead.
        for (status, expected) in [
            (STATUS_STOPPED, DownloadItemState::Paused),
            (STATUS_CHECK_WAIT, DownloadItemState::Verifying),
            (STATUS_CHECK, DownloadItemState::Verifying),
            (STATUS_QUEUED, DownloadItemState::Queued),
            (STATUS_DOWNLOADING, DownloadItemState::Downloading),
            (STATUS_SEEDING_WAIT, DownloadItemState::Downloading),
            (STATUS_SEEDING, DownloadItemState::Downloading),
        ] {
            assert_eq!(
                map_state(&queued_torrent(status)),
                expected,
                "queued torrent in status {status}"
            );
            assert_eq!(
                map_state(&downloading_torrent(status)),
                expected,
                "downloading torrent in status {status}"
            );
        }
    }

    #[test]
    fn a_finished_torrent_maps_sonarrs_completed_status_table() {
        // Sonarr: Stopped/Queued/SeedingWait/Seeding are Completed, and
        // Check/CheckWait stay Downloading. Scryer reports an actively seeding
        // torrent as `Seeding`, which the core maps to the same queue state.
        for (status, expected) in [
            (STATUS_STOPPED, DownloadItemState::Completed),
            (STATUS_CHECK_WAIT, DownloadItemState::Verifying),
            (STATUS_CHECK, DownloadItemState::Verifying),
            (STATUS_QUEUED, DownloadItemState::Completed),
            (STATUS_SEEDING_WAIT, DownloadItemState::Completed),
            (STATUS_SEEDING, DownloadItemState::Seeding),
        ] {
            assert_eq!(
                map_state(&complete_torrent(status)),
                expected,
                "completed torrent in status {status}"
            );
        }
    }

    #[test]
    fn a_seeding_torrent_with_data_missing_is_still_downloading() {
        // The port used to answer `Completed` for status 6 regardless of
        // `leftUntilDone` (TransmissionFixture.cs:165, 179).
        let torrent = TransmissionTorrent {
            left_until_done: 100,
            is_finished: false,
            ..complete_torrent(STATUS_SEEDING)
        };
        assert_eq!(map_state(&torrent), DownloadItemState::Downloading);
        assert!(!is_completed(&torrent));
    }

    #[test]
    fn a_magnet_without_metadata_is_queued() {
        let torrent = TransmissionTorrent {
            total_size: 0,
            left_until_done: 100,
            ..queued_torrent(STATUS_DOWNLOADING)
        };
        assert_eq!(map_state(&torrent), DownloadItemState::Queued);
        let item = item(&SessionConfig::default(), torrent);
        assert_eq!(
            item.torrent.and_then(|torrent| torrent.metadata_only),
            Some(true)
        );
    }

    #[test]
    fn an_error_string_wins_over_every_status() {
        for status in 0..=6 {
            let torrent = TransmissionTorrent {
                error_string: "Error".to_string(),
                ..complete_torrent(status)
            };
            assert_eq!(map_state(&torrent), DownloadItemState::Warning);
        }
        let item = item(
            &SessionConfig::default(),
            TransmissionTorrent {
                error_string: "  No data found!  ".to_string(),
                ..downloading_torrent(STATUS_STOPPED)
            },
        );
        assert_eq!(item.message.as_deref(), Some("No data found!"));
        assert_eq!(
            item.torrent.and_then(|torrent| torrent.status_reason),
            Some("No data found!".to_string())
        );
    }

    #[test]
    fn an_undocumented_status_code_keeps_polling_instead_of_warning() {
        let torrent = TransmissionTorrent {
            status: 42,
            ..downloading_torrent(STATUS_DOWNLOADING)
        };
        assert_eq!(map_state(&torrent), DownloadItemState::Downloading);
    }

    // -----------------------------------------------------------------------
    // ETA (TransmissionFixture.cs:289-318)
    // -----------------------------------------------------------------------

    #[test]
    fn a_negative_eta_is_unknown() {
        assert_eq!(normalized_eta(-1), None);
        assert_eq!(normalized_eta(-2), None);
    }

    #[test]
    fn an_eta_that_overflows_seconds_is_read_as_milliseconds() {
        assert_eq!(normalized_eta(2_147_483_648), Some(2_147_483_648));
        assert_eq!(normalized_eta(2_147_483_648_000), Some(2_147_483_648));
        assert_eq!(normalized_eta(MAX_ETA_SECONDS), Some(MAX_ETA_SECONDS));
        assert_eq!(normalized_eta(0), Some(0));
    }

    // -----------------------------------------------------------------------
    // Version parsing / label gate (TransmissionFixture.cs:276-287)
    // -----------------------------------------------------------------------

    #[test]
    fn only_the_version_number_is_read() {
        for raw in ["2.84 ()", "2.84+ ()", "2.84 (other info)", "2.84 (2.84)"] {
            let version = parse_client_version(raw).unwrap_or_else(|| panic!("parse {raw}"));
            assert_eq!(version.to_string(), "2.84", "parsing {raw}");
            assert!(version.at_least(MINIMUM_SUPPORTED_VERSION));
            assert!(!version.at_least(LABEL_SUPPORT_VERSION));
        }

        let version = parse_client_version("4.0.6").expect("parse 4.0.6");
        assert_eq!(version.to_string(), "4.0.6");
        assert!(version.at_least(LABEL_SUPPORT_VERSION));
    }

    #[test]
    fn an_unreadable_version_is_not_a_version() {
        // `Version.Parse` needs major.minor, rejects empty components, and
        // stops at four.
        for raw in ["", "unknown", "3", "2.", "1.2.3.4.5", "(4.0.6)"] {
            assert!(parse_client_version(raw).is_none(), "parsing {raw}");
        }
    }

    #[test]
    fn a_version_below_four_is_ordered_below_the_label_floor() {
        assert!(
            parse_client_version("3.00")
                .unwrap()
                .at_least(MINIMUM_SUPPORTED_VERSION)
        );
        assert!(
            !parse_client_version("3.00")
                .unwrap()
                .at_least(LABEL_SUPPORT_VERSION)
        );
        assert!(
            !parse_client_version("2.39")
                .unwrap()
                .at_least(MINIMUM_SUPPORTED_VERSION)
        );
        assert!(
            parse_client_version("2.40")
                .unwrap()
                .at_least(MINIMUM_SUPPORTED_VERSION)
        );
        assert!(
            parse_client_version("4.1.0")
                .unwrap()
                .at_least(LABEL_SUPPORT_VERSION)
        );
    }

    // -----------------------------------------------------------------------
    // Scope (TransmissionFixture.cs:220-256)
    // -----------------------------------------------------------------------

    #[test]
    fn a_category_scopes_by_label_when_the_client_has_labels() {
        let config = TransmissionConfig {
            category: "sonarr".to_string(),
            ..base_config()
        };
        let labelled = TransmissionTorrent {
            labels: vec!["SONARR".to_string()],
            ..downloading_torrent(STATUS_DOWNLOADING)
        };
        let other = TransmissionTorrent {
            labels: vec!["radarr".to_string()],
            ..downloading_torrent(STATUS_DOWNLOADING)
        };
        assert!(torrent_matches_scope(&config, &labelled, true));
        assert!(!torrent_matches_scope(&config, &other, true));
    }

    #[test]
    fn without_label_support_a_category_scopes_by_path_component() {
        let config = TransmissionConfig {
            category: "sonarr".to_string(),
            ..base_config()
        };
        // The very torrent a 4.0 client would have matched by label is matched
        // by its download directory instead, and a label alone is not enough.
        let inside = TransmissionTorrent {
            download_dir: "C:/Downloads/Finished/transmission/sonarr".to_string(),
            ..downloading_torrent(STATUS_DOWNLOADING)
        };
        let outside = TransmissionTorrent {
            labels: vec!["sonarr".to_string()],
            download_dir: "somepath".to_string(),
            ..queued_torrent(STATUS_QUEUED)
        };
        assert!(torrent_matches_scope(&config, &inside, false));
        assert!(!torrent_matches_scope(&config, &outside, false));
        // With labels enabled the same pair flips, which is exactly the bug the
        // version gate closes.
        assert!(torrent_matches_scope(&config, &outside, true));
    }

    #[test]
    fn a_directory_scopes_by_containment() {
        let config = TransmissionConfig {
            directory: "C:/Downloads/Finished/sonarr".to_string(),
            ..base_config()
        };
        let inside = TransmissionTorrent {
            download_dir: "C:/Downloads/Finished/sonarr/subdir".to_string(),
            ..downloading_torrent(STATUS_DOWNLOADING)
        };
        assert!(torrent_matches_scope(&config, &inside, true));
        assert!(!torrent_matches_scope(
            &config,
            &queued_torrent(STATUS_QUEUED),
            true
        ));
    }

    #[test]
    fn without_a_category_or_directory_every_torrent_is_in_scope() {
        assert!(torrent_matches_scope(
            &base_config(),
            &queued_torrent(STATUS_QUEUED),
            true
        ));
    }

    // -----------------------------------------------------------------------
    // Reported category (TransmissionBase.cs:82)
    // -----------------------------------------------------------------------

    #[test]
    fn the_reported_category_is_the_matching_label_in_the_clients_casing() {
        let config = TransmissionConfig {
            category: "scryer-tv".to_string(),
            ..base_config()
        };
        let torrent = TransmissionTorrent {
            // The isolation value sorts first; reporting `labels.first()` used
            // to report it as the category.
            labels: vec!["Archive".to_string(), "Scryer-TV".to_string()],
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            reported_category(&config, &torrent).as_deref(),
            Some("Scryer-TV")
        );
        assert_eq!(
            torrent_to_completed(&config, torrent).category.as_deref(),
            Some("Scryer-TV")
        );

        let unlabelled = TransmissionTorrent {
            labels: vec!["Archive".to_string()],
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(reported_category(&config, &unlabelled), None);
        assert_eq!(reported_category(&base_config(), &unlabelled), None);
    }

    // -----------------------------------------------------------------------
    // Output paths (TransmissionBase.cs:259-262, Vuze.cs:39-56)
    // -----------------------------------------------------------------------

    #[test]
    fn the_output_path_is_the_download_directory_plus_the_name() {
        let torrent = TransmissionTorrent {
            download_dir: "C:/Downloads/Finished/transmission/".to_string(),
            name: "Title.S01E01: Pilot".to_string(),
            file_count: Some(3),
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            output_path(&torrent),
            "C:/Downloads/Finished/transmission/Title.S01E01_ Pilot"
        );
    }

    #[test]
    fn a_vuze_payload_uses_vuzes_own_layout() {
        // Multi-file: `downloadDir` already is the job folder.
        let multi = TransmissionTorrent {
            download_dir: "/downloads/Title.S01".to_string(),
            name: "Title.S01".to_string(),
            vuze_file_count: Some(4),
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(output_path(&multi), "/downloads/Title.S01");

        // Single file sitting in the root folder.
        let single = TransmissionTorrent {
            download_dir: "/downloads".to_string(),
            name: "Title.S01E01.mkv".to_string(),
            vuze_file_count: Some(1),
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(output_path(&single), "/downloads/Title.S01E01.mkv");

        // A Transmission payload with the same numbers keeps Transmission's rule.
        let transmission = TransmissionTorrent {
            download_dir: "/downloads/Title.S01".to_string(),
            name: "Title.S01".to_string(),
            file_count: Some(4),
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(output_path(&transmission), "/downloads/Title.S01/Title.S01");
    }

    #[test]
    fn the_output_kind_follows_the_file_count() {
        let kind = |torrent: TransmissionTorrent| {
            torrent_to_completed(&base_config(), torrent).output_kind
        };
        assert_eq!(
            kind(TransmissionTorrent {
                file_count: Some(1),
                ..complete_torrent(STATUS_STOPPED)
            }),
            Some(PluginDownloadOutputKind::File)
        );
        assert_eq!(
            kind(TransmissionTorrent {
                file_count: Some(9),
                ..complete_torrent(STATUS_STOPPED)
            }),
            Some(PluginDownloadOutputKind::Directory)
        );
        assert_eq!(
            kind(complete_torrent(STATUS_STOPPED)),
            Some(PluginDownloadOutputKind::Unknown)
        );
    }

    // -----------------------------------------------------------------------
    // Download directory (TransmissionFixture.cs:81-144)
    // -----------------------------------------------------------------------

    #[test]
    fn the_download_root_follows_sonarrs_directory_and_category_rules() {
        let session = |download_dir: &str| SessionConfig {
            download_dir: Some(download_dir.to_string()),
            ..SessionConfig::default()
        };

        let forced = TransmissionConfig {
            directory: "C:/Downloads/Finished/sonarr".to_string(),
            ..base_config()
        };
        assert_eq!(
            effective_output_root(&forced, &session("C:/Downloads/Finished/transmission")),
            Some("C:/Downloads/Finished/sonarr".to_string())
        );

        let categorised = TransmissionConfig {
            category: "sonarr".to_string(),
            ..base_config()
        };
        assert_eq!(
            effective_output_root(&categorised, &session("C:/Downloads/Finished/transmission")),
            Some("C:/Downloads/Finished/transmission/sonarr".to_string())
        );
        assert_eq!(
            effective_output_root(
                &categorised,
                &session("C:/Downloads/Finished/transmission/")
            ),
            Some("C:/Downloads/Finished/transmission/sonarr".to_string())
        );

        assert_eq!(
            effective_output_root(
                &base_config(),
                &session("C:/Downloads/Finished/transmission")
            ),
            Some("C:/Downloads/Finished/transmission".to_string())
        );
        assert_eq!(
            effective_output_root(&base_config(), &SessionConfig::default()),
            None
        );
    }

    // -----------------------------------------------------------------------
    // Settings validation (TransmissionSettings.cs:10-24)
    // -----------------------------------------------------------------------

    #[test]
    fn the_category_charset_matches_sonarrs_validator() {
        for valid in ["sonarr", "scryer-tv", ".hidden", "", "-"] {
            assert!(is_valid_category(valid), "{valid} should be valid");
        }
        for invalid in ["tv2", "tv sonarr", "tv.sonarr", "..tv", "TV_SONARR"] {
            assert!(!is_valid_category(invalid), "{invalid} should be invalid");
        }
        // The validator is case-insensitive.
        assert!(is_valid_category("Scryer-TV"));
    }

    #[test]
    fn a_category_and_a_directory_together_are_refused() {
        let both = TransmissionConfig {
            category: "scryer-tv".to_string(),
            directory: "/downloads/tv".to_string(),
            ..base_config()
        };
        assert!(conflicting_settings(&both).is_some());
        assert!(settings_problem(&both).is_some());

        let directory_only = TransmissionConfig {
            directory: "/downloads/tv".to_string(),
            ..base_config()
        };
        assert!(conflicting_settings(&directory_only).is_none());
        assert!(settings_problem(&directory_only).is_none());
    }

    #[test]
    fn an_invalid_category_is_a_settings_problem_but_not_an_add_blocker() {
        // A label with a digit works perfectly well in Transmission, so it is
        // surfaced through test_connection and the status warnings rather than
        // stranding an existing client's grabs.
        let digits = TransmissionConfig {
            category: "tv2".to_string(),
            ..base_config()
        };
        assert!(settings_problem(&digits).is_some());
        assert!(conflicting_settings(&digits).is_none());
    }

    // -----------------------------------------------------------------------
    // Post-import handoff (Transmission.cs:36-73)
    // -----------------------------------------------------------------------

    #[test]
    fn post_import_configuration_is_non_destructive_and_migrates_legacy_values() {
        let fields = config_fields();
        assert!(fields.iter().all(|field| field.key != "post_import_action"));
        let label_after_import = fields
            .iter()
            .find(|field| field.key == "label_after_import")
            .expect("label-after-import field");
        assert_eq!(label_after_import.field_type, ConfigFieldType::Bool);
        assert_eq!(label_after_import.default_value.as_deref(), Some("true"));

        assert!(!resolve_label_after_import(None, Some("retain")));
        for legacy in ["remove", "remove_with_data"] {
            assert!(resolve_label_after_import(None, Some(legacy)));
        }
        assert!(resolve_label_after_import(None, None));
        assert!(!resolve_label_after_import(Some("false"), Some("remove")));
        assert!(resolve_label_after_import(Some("true"), Some("retain")));
    }

    #[test]
    fn the_descriptor_advertises_the_non_destructive_mark() {
        let descriptor: serde_json::Value =
            serde_json::from_str(&scryer_describe(String::new()).unwrap()).unwrap();
        assert_eq!(
            descriptor["provider"]["capabilities"]["mark_imported_non_destructive"],
            true
        );
        assert!(
            functions().mark_imported_non_destructive.is_some(),
            "the function table must route the core's post-import mark"
        );
    }

    #[test]
    fn the_post_import_label_replaces_the_scope_label() {
        let existing = vec!["Scryer-TV".to_string(), "keep-me".to_string()];
        let labels = swap_labels(&existing, Some("scryer-tv"), "imported");
        assert_eq!(labels, vec!["keep-me".to_string(), "imported".to_string()]);
        assert!(!same_label_set(&existing, &labels));
    }

    #[test]
    fn an_imported_label_equal_to_the_category_is_left_alone() {
        // Sonarr guards the whole swap on `TvImportedCategory != TvCategory`.
        let existing = vec!["scryer-tv".to_string()];
        let labels = swap_labels(&existing, Some("scryer-tv"), "Scryer-TV");
        assert_eq!(labels, existing);
        assert!(same_label_set(&existing, &labels));
    }

    #[test]
    fn the_scope_label_comes_from_the_cores_routing_before_the_configured_category() {
        let config = TransmissionConfig {
            category: "scryer-tv".to_string(),
            ..base_config()
        };
        let routed: PluginDownloadClientMarkImportedRequest = serde_json::from_str(
            r#"{"client_item_id":"abc","category":"tracked","post_import_isolation":[{"mode":"tag","value":"routed"}]}"#,
        )
        .unwrap();
        assert_eq!(
            post_import_scope_label(&config, &routed).as_deref(),
            Some("routed")
        );

        let tracked: PluginDownloadClientMarkImportedRequest =
            serde_json::from_str(r#"{"client_item_id":"abc","category":"tracked"}"#).unwrap();
        assert_eq!(
            post_import_scope_label(&config, &tracked).as_deref(),
            Some("tracked")
        );

        let bare: PluginDownloadClientMarkImportedRequest =
            serde_json::from_str(r#"{"client_item_id":"abc"}"#).unwrap();
        assert_eq!(
            post_import_scope_label(&config, &bare).as_deref(),
            Some("scryer-tv")
        );
        assert_eq!(post_import_scope_label(&base_config(), &bare), None);
    }

    // -----------------------------------------------------------------------
    // Add
    // -----------------------------------------------------------------------

    fn add_request(routing: &str) -> PluginDownloadClientAddRequest {
        serde_json::from_str(&format!(
            r#"{{
                "source":{{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn:btih:abc"}},
                "release":{{"release_title":"Example"}},
                "title":{{"title_name":"Example","media_facet":"series","tags":[]}},
                "routing":{routing}
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn the_category_leads_the_labels_and_the_routed_value_rides_along() {
        let config = TransmissionConfig {
            category: "scryer-tv".to_string(),
            ..base_config()
        };
        assert_eq!(
            labels_for_request(&config, &add_request(r#"{"isolation_value":"series"}"#)),
            vec!["scryer-tv".to_string(), "series".to_string()]
        );
        // A routed value that *is* the category is not duplicated, whatever its casing.
        assert_eq!(
            labels_for_request(&config, &add_request(r#"{"isolation_value":"Scryer-TV"}"#)),
            vec!["scryer-tv".to_string()]
        );
        assert_eq!(
            labels_for_request(&config, &add_request("{}")),
            vec!["scryer-tv".to_string()]
        );
        assert!(labels_for_request(&base_config(), &add_request("{}")).is_empty());
    }

    #[test]
    fn an_explicit_queue_placement_beats_the_configured_priority() {
        let first = TransmissionConfig {
            recent_priority: PluginTorrentQueuePlacement::First,
            ..base_config()
        };
        let mut request = add_request("{}");
        assert!(!should_move_to_top(&first, &request));

        request.release.is_recent = Some(true);
        assert!(should_move_to_top(&first, &request));

        request.torrent = Some(scryer_plugin_sdk::PluginTorrentOptions {
            queue_placement: Some(PluginTorrentQueuePlacement::Last),
            ..Default::default()
        });
        assert!(!should_move_to_top(&first, &request));
    }

    // -----------------------------------------------------------------------
    // Completion time
    // -----------------------------------------------------------------------

    #[test]
    fn done_date_becomes_an_rfc_3339_completion_time() {
        assert_eq!(unix_to_rfc3339(0), None);
        assert_eq!(unix_to_rfc3339(-5), None);
        assert_eq!(
            unix_to_rfc3339(1_700_000_000).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );

        let torrent = TransmissionTorrent {
            done_date: 1_700_000_000,
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            item(&SessionConfig::default(), torrent)
                .completed_at
                .as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert_eq!(
            item(&SessionConfig::default(), complete_torrent(STATUS_STOPPED)).completed_at,
            None
        );
    }

    // -----------------------------------------------------------------------
    // Errors
    // -----------------------------------------------------------------------

    #[test]
    fn http_statuses_carry_sonarrs_distinctions_as_typed_codes() {
        let classify = |status| classify_http_status(status, None, "");
        for (status, code) in [
            (401_u16, PluginErrorCode::AuthFailed),
            (403, PluginErrorCode::AuthFailed),
            (404, PluginErrorCode::InvalidConfig),
            (409, PluginErrorCode::InvalidConfig),
            (429, PluginErrorCode::Temporary),
            (500, PluginErrorCode::Temporary),
            (503, PluginErrorCode::Temporary),
            (418, PluginErrorCode::Permanent),
        ] {
            let error = classify(status).unwrap_or_else(|| panic!("status {status} is an error"));
            assert_eq!(error.code, code, "status {status}");
        }
        assert!(classify(200).is_none());
        assert!(classify(204).is_none());

        // Sonarr's 403 names the RPC whitelist, which is the actual fix.
        assert!(
            classify(403)
                .unwrap()
                .public_message
                .contains("RPC whitelist")
        );
    }

    #[test]
    fn a_redirect_reports_where_it_went_instead_of_a_parse_failure() {
        // The host never follows redirects for plugin HTTP, so a login page
        // arrives as a 3xx and must not be mistaken for a broken RPC response.
        let redirected =
            classify_http_status(301, Some("http://downloader.example/login"), "").unwrap();
        assert_eq!(redirected.code, PluginErrorCode::InvalidConfig);
        assert_eq!(
            redirected.public_message,
            "Remote site redirected to http://downloader.example/login"
        );
        assert_eq!(
            classify_http_status(302, None, "").unwrap().code,
            PluginErrorCode::InvalidConfig
        );
    }

    #[test]
    fn transport_failures_are_classified_by_what_the_host_could_tell_us() {
        assert_eq!(
            classify_transport_error("timeout").code,
            PluginErrorCode::Temporary
        );
        assert_eq!(
            classify_transport_error("invalid peer certificate: UnknownIssuer").code,
            PluginErrorCode::UpstreamUnavailable
        );
        assert!(
            classify_transport_error("invalid peer certificate: UnknownIssuer")
                .public_message
                .contains("certificate validation failed")
        );
        let refused = classify_transport_error("error sending request: connection refused");
        assert_eq!(refused.code, PluginErrorCode::UpstreamUnavailable);
        assert_eq!(
            refused.debug_message.as_deref(),
            Some("error sending request: connection refused")
        );
    }

    // -----------------------------------------------------------------------
    // Seeding (the seeding-audit contract; unchanged semantics)
    // -----------------------------------------------------------------------

    #[test]
    fn can_remove_is_false_while_downloading() {
        let item = item(
            &SessionConfig::default(),
            downloading_torrent(STATUS_DOWNLOADING),
        );
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
            derive_can_remove(&SessionConfig::default(), &torrent, true, Some(0.5)),
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
            derive_can_remove(&SessionConfig::default(), &torrent, true, Some(1.5)),
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
            derive_can_remove(&SessionConfig::default(), &torrent, true, Some(9.0)),
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
        assert_eq!(derive_can_remove(&session, &torrent, true, Some(9.0)), None);
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
            derive_can_remove(&session, &torrent, true, Some(1.2)),
            Some(true)
        );
        assert_eq!(
            derive_can_remove(&session, &torrent, true, Some(0.2)),
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
            derive_can_remove(&SessionConfig::default(), &torrent, true, Some(4.0)),
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
        assert_eq!(item.state, DownloadItemState::Seeding);
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
    fn observed_seed_state_comes_from_the_torrent_get_payload() {
        let torrent: TransmissionTorrent = serde_json::from_str(
            r#"{"hashString":"a1","name":"n","secondsSeeding":7200,"uploadedEver":300,"downloadedEver":200,"rateDownload":1024,"rateUpload":512}"#,
        )
        .unwrap();
        let torrent = item(&SessionConfig::default(), torrent).torrent.unwrap();
        assert_eq!(torrent.seed_time_seconds, Some(7_200));
        assert_eq!(torrent.seed_ratio, Some(1.5));
        assert_eq!(torrent.download_rate_bytes_per_second, Some(1_024));
        assert_eq!(torrent.upload_rate_bytes_per_second, Some(512));
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
            derive_can_remove(&session, &unmet, true, Some(9.0)),
            Some(false)
        );
        let met = TransmissionTorrent {
            seconds_seeding: 3_600,
            ..unmet
        };
        assert_eq!(
            derive_can_remove(&session, &met, true, Some(9.0)),
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
        assert_eq!(derive_can_remove(&session, &torrent, true, Some(9.0)), None);
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
