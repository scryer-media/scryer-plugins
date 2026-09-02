//! Flood download client.
//!
//! Reconciled against Sonarr's `NzbDrone.Core/Download/Clients/Flood`
//! (`Flood.cs`, `FloodProxy.cs`, `FloodSettings.cs`) **and** against Flood's
//! own current sources, which are the second — and where they disagree, the
//! authoritative — source of truth:
//!
//! - `server/routes/api/torrents.ts` (route table, request/response schemas),
//! - `server/routes/api/auth.ts` + `server/util/authUtil.ts` (JWT cookie),
//! - `shared/schema/api/torrents.ts` (add/tag/delete bodies),
//! - `shared/types/Torrent.ts` and `shared/constants/torrentStatusMap.ts`
//!   (the torrent shape and the full status vocabulary),
//! - `server/services/{rTorrent,qBittorrent}/clientGatewayService.ts`
//!   (what each backend actually returns, and in which hash casing).
//!
//! Read at Flood `master`, release line 4.16.x (v4.16.1, 2026-08-05).

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use scryer_plugin_pdk::*;
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
use sha1::{Digest, Sha1};

const COOKIE_VAR_KEY: &str = "flood.cookie";
const SEED_CONFIG_VAR_PREFIX: &str = "flood.seed_config.";
/// Resolved content paths of a *completed* torrent. A finished torrent's file
/// list cannot change, so one `GET /torrents/{hash}/contents` per torrent for
/// the lifetime of the plugin instance replaces one per completed torrent per
/// poll (the bridge calls `list_completed` on every queue poll).
const CONTENTS_VAR_PREFIX: &str = "flood.contents.";

macro_rules! warn_log {
    ($($argument:tt)*) => {
        scryer_plugin_pdk::log::log(
            scryer_plugin_pdk::log::LogLevel::Warn,
            &format!($($argument)*),
        )
    };
}

#[derive(Debug, Clone)]
struct FloodConfig {
    host: String,
    port: String,
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

/// `TorrentProperties` (`shared/types/Torrent.ts`). Every numeric field is
/// optional here because Flood 4.x grew them over time and an older server must
/// keep working: `dateFinished` arrived in 4.5, `isPrivate`/`isInitialSeeding`
/// later still, and a missing field must read as "unknown", never as zero.
#[derive(Default, Deserialize, Clone)]
struct FloodTorrent {
    #[serde(default, rename = "bytesDone")]
    bytes_done: i64,
    #[serde(default)]
    directory: String,
    /// Seconds, `-1` for "infinity" (`shared/types/Torrent.ts`).
    #[serde(default)]
    eta: i64,
    /// The torrent's own hash as Flood knows it; the list is also keyed by it.
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    message: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    ratio: f64,
    #[serde(default, rename = "sizeBytes")]
    size_bytes: i64,
    #[serde(default, rename = "percentComplete")]
    percent_complete: Option<f64>,
    #[serde(default)]
    status: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default, rename = "dateFinished")]
    date_finished: Option<i64>,
    #[serde(default, rename = "upTotal")]
    up_total: Option<i64>,
    #[serde(default, rename = "downTotal")]
    down_total: Option<i64>,
    #[serde(default, rename = "upRate")]
    up_rate: Option<i64>,
    #[serde(default, rename = "downRate")]
    down_rate: Option<i64>,
    /// Flood surfaces libtorrent's private flag as `isPrivate`; `None` when the
    /// running Flood build does not report it.
    #[serde(default, rename = "isPrivate")]
    is_private: Option<bool>,
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

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// `Err(Error::msg(..))` reaches the host as `PluginErrorCode::Temporary`, so a
/// wrong password would be retried forever as a transient fault. Every failure
/// this plugin can name therefore carries its own code (`00-common.md` rule 4),
/// mirroring `FloodProxy.HandleRequest` (`FloodProxy.cs:62-83`) and
/// `Flood.Test` (`Flood.cs:260-274`), which splits authentication failures onto
/// the `Password` field and everything else onto `Host`.
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
                // `POST /api/torrents/stop` and `/start` (Flood's own routes;
                // Sonarr's client never calls them).
                pause: true,
                resume: true,
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
                // SDK 3.10 addition. `false` is the SDK's own default and therefore exactly
                // what this client's pre-3.10 descriptor already meant to a 3.10 host;
                // advertising category-scoped feedback would be a behaviour change, not a
                // transport one, so it stays off across the component migration.
                category_scoped_feedback: false,
                // The core's only caller of the post-import handoff is the
                // non-destructive one (`result_state.rs`), and it is wired
                // below; the destructive export runs the same body.
                mark_imported_non_destructive: true,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

// ---------------------------------------------------------------------------
// Add
// ---------------------------------------------------------------------------

pub fn scryer_download_add(input: String) -> FnResult<String> {
    respond(add(&input))
}

fn add(input: &str) -> Result<PluginDownloadClientAddResponse, PluginError> {
    let request: PluginDownloadClientAddRequest = parse_request(input)?;
    let config = FloodConfig::from_host();

    // Sonarr's core resolves the hash before it ever reaches the client
    // (`TorrentClientBase.cs`, `AddFromMagnetLink`/`AddFromTorrentFile` are
    // handed one). Flood's add routes answer with an array of hashes
    // (`server/routes/api/torrents.ts:150-255`), but only the qBittorrent
    // backend actually fills it — the rTorrent gateway returns `[]` and the
    // route answers 202 (`server/services/rTorrent/clientGatewayService.ts:238`,
    // `:277-341`). So derive locally, and prefer whatever Flood reports.
    let derived = derive_info_hash(&request);

    let mut body = serde_json::Map::new();
    body.insert(
        "tags".to_string(),
        serde_json::Value::Array(
            tags_for_request(&config, &request)
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    if let Some(destination) = request
        .routing
        .download_directory
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| (!config.destination.is_empty()).then(|| config.destination.clone()))
    {
        body.insert(
            "destination".to_string(),
            serde_json::Value::String(destination),
        );
    }
    // Sent explicitly either way, as Sonarr does (`FloodProxy.cs`): leaving it
    // out would hand the decision to Flood's own default.
    body.insert(
        "start".to_string(),
        serde_json::Value::Bool(config.start_on_add),
    );

    let response = if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        body.insert("files".to_string(), serde_json::json!([bytes]));
        api_call(
            &config,
            "POST",
            "/torrents/add-files",
            Some(serde_json::Value::Object(body)),
        )?
    } else if let Some(source) = source_url(&request) {
        body.insert("urls".to_string(), serde_json::json!([source]));
        api_call(
            &config,
            "POST",
            "/torrents/add-urls",
            Some(serde_json::Value::Object(body)),
        )?
    } else {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "download source is missing",
        ));
    };

    let hash = reported_add_hash(&response).or(derived).ok_or_else(|| {
        detailed_error(
            PluginErrorCode::Permanent,
            "Flood accepted the release but neither the release nor Flood reported an info hash, \
             so Scryer cannot track it. Remove it in Flood and re-grab from an indexer that \
             publishes a magnet link or a torrent file.",
            format!("add response: {}", truncate(&response)),
        )
    })?;

    store_seed_config(&hash, &request)?;
    Ok(PluginDownloadClientAddResponse {
        client_item_id: hash.clone(),
        info_hash: Some(hash),
    })
}

/// `200`/`202`/`207` all carry `hashesResponseSchema`, an array of strings
/// (`server/routes/api/torrents.ts:104`). Anything else — an older Flood that
/// answered `{}` or an empty body — leaves the hash to local derivation.
fn reported_add_hash(body: &str) -> Option<String> {
    serde_json::from_str::<Vec<String>>(body)
        .ok()?
        .into_iter()
        .map(|value| normalize_hash(&value))
        .find(|value| value.len() == 40)
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    respond(list_queue())
}

fn list_queue() -> Result<Vec<PluginDownloadItem>, PluginError> {
    let config = FloodConfig::from_host();
    let now = now_unix_seconds();
    Ok(list_torrents(&config)?
        .into_iter()
        .filter(|(_, torrent)| matches_scope(&config, torrent))
        .map(|(key, torrent)| {
            let client_hash = client_hash_of(&key, &torrent);
            torrent_to_item(&client_hash, torrent, now)
        })
        .collect())
}

/// Flood keys `/api/torrents` by the torrent's own hash and repeats it in
/// `TorrentProperties.hash` (`shared/types/Torrent.ts`). Prefer the property:
/// it is the value the backend itself produced, and it is what the action
/// routes expect back.
fn client_hash_of(key: &str, torrent: &FloodTorrent) -> String {
    torrent
        .hash
        .as_deref()
        .and_then(trimmed_non_empty)
        .unwrap_or_else(|| key.to_string())
}

/// Flood keeps no failed-download history: `/api/torrents` is the whole world,
/// and the bridge only ever reads `Failed`/`Error` rows out of this call
/// (`pdk/scryer-plugin-pdk/src/download_client_bridge.rs:173-196`), which this
/// client's status table never produces. Re-fetching the full list here bought
/// nothing but a second `/torrents` round trip on every queue poll.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    respond(Ok::<Vec<PluginDownloadItem>, PluginError>(Vec::new()))
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    respond(list_completed())
}

fn list_completed() -> Result<Vec<PluginCompletedDownload>, PluginError> {
    let config = FloodConfig::from_host();
    let mut downloads = Vec::new();
    for (key, torrent) in list_torrents(&config)? {
        if !matches_scope(&config, &torrent) || !is_completed(&torrent) {
            continue;
        }
        let client_hash = client_hash_of(&key, &torrent);
        let content_paths = completed_content_paths(&config, &client_hash)?;
        if content_paths.is_empty() {
            // Sonarr raises `DownloadClientUnavailableException` here
            // (`Flood.cs:196-199`). Failing the whole poll would hide every
            // other finished download, so drop this one and let the next poll
            // retry it — the same net effect, one torrent wide.
            warn_log!(
                "Flood returned no contents for torrent \"{client_hash}\"; skipping this poll."
            );
            continue;
        }
        downloads.push(torrent_to_completed(&client_hash, torrent, content_paths));
    }
    Ok(downloads)
}

// ---------------------------------------------------------------------------
// Control
// ---------------------------------------------------------------------------

pub fn scryer_download_control(input: String) -> FnResult<String> {
    respond(control(&input))
}

fn control(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientControlRequest = parse_request(input)?;
    let config = FloodConfig::from_host();
    let hash = normalize_hash(&request.client_item_id);
    if hash.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ));
    }
    match request.action {
        DownloadControlAction::Remove => {
            let Some(client_hash) = resolve_client_hash(&config, &hash)? else {
                warn_log!("Could not find torrent with hash \"{hash}\" in Flood.");
                forget_torrent_state(&hash);
                return Ok(());
            };
            api_call(
                &config,
                "POST",
                "/torrents/delete",
                Some(serde_json::json!({
                    "hashes": [client_hash],
                    "deleteData": request.remove_data,
                })),
            )?;
            forget_torrent_state(&hash);
        }
        DownloadControlAction::Pause | DownloadControlAction::Resume => {
            let Some(client_hash) = resolve_client_hash(&config, &hash)? else {
                return Err(plugin_error(
                    PluginErrorCode::Permanent,
                    "download item was not found",
                ));
            };
            let route = control_route(request.action).expect("pause and resume have routes");
            api_call(
                &config,
                "POST",
                route,
                Some(serde_json::json!({ "hashes": [client_hash] })),
            )?;
        }
        DownloadControlAction::ForceStart => {
            return Err(plugin_error(
                PluginErrorCode::Unsupported,
                "Flood has no force-start; its start route honours the backend's queue",
            ));
        }
    }
    Ok(())
}

/// `POST /api/torrents/start` and `/stop` take `{hashes: string[]}`
/// (`shared/schema/api/torrents.ts`, `server/routes/api/torrents.ts`). Sonarr's
/// client does not use them; Scryer routes pause/resume to any client that
/// advertises them, and Flood documents both.
fn control_route(action: DownloadControlAction) -> Option<&'static str> {
    match action {
        DownloadControlAction::Pause => Some("/torrents/stop"),
        DownloadControlAction::Resume => Some("/torrents/start"),
        DownloadControlAction::Remove | DownloadControlAction::ForceStart => None,
    }
}

// ---------------------------------------------------------------------------
// Post-import handoff
// ---------------------------------------------------------------------------

/// The destructive mark has no core caller, and removing a finished torrent at
/// import time is what Scryer's seeding gate exists to prevent, so it runs the
/// same non-destructive body.
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    respond(mark_imported(&input))
}

pub fn scryer_download_mark_imported_non_destructive(input: String) -> FnResult<String> {
    respond(mark_imported(&input))
}

/// `Flood.MarkItemAsImported` (`Flood.cs:224-237`) in Scryer's shape: union the
/// configured post-import tags onto the torrent's existing tags, never remove
/// anything, and treat a torrent that is gone as a warning rather than a
/// failure. Adding the tags is what takes the torrent out of `matches_scope`,
/// which is Flood's equivalent of a category swap.
fn mark_imported(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientMarkImportedRequest = parse_request(input)?;
    let config = FloodConfig::from_host();
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

    let wanted = post_import_tags_to_apply(&config, &request);
    if wanted.is_empty() {
        return Ok(());
    }

    let torrents = list_torrents(&config)?;
    let Some((key, torrent)) = torrents
        .into_iter()
        .find(|(key, _)| normalize_hash(key) == hash)
    else {
        warn_log!("Could not find torrent with hash \"{hash}\" in Flood.");
        return Ok(());
    };
    let client_hash = client_hash_of(&key, &torrent);

    let tags = merge_tags(&torrent.tags, &wanted);
    if same_tag_set(&torrent.tags, &tags) {
        return Ok(());
    }

    api_call(
        &config,
        "PATCH",
        "/torrents/tags",
        Some(serde_json::json!({ "hashes": [client_hash], "tags": tags })),
    )?;
    Ok(())
}

/// The tags to APPLY are always the plugin's configured `post_import_tags`.
///
/// `request.post_import_isolation` is **not** a new tag: the core builds it
/// with `build_isolation_entries(request.category)`
/// (`crates/scryer-plugins/src/download_client_adapter.rs:683-700` on
/// `release-0.19.8`), i.e. the download's own grab tag replicated across
/// Category/Tag/Label/View. It is only useful here as Sonarr's
/// "imported != grabbed" guard: a configured post-import tag that *is* the grab
/// tag is already on the torrent, so applying it is a no-op — and Flood's own
/// settings validator forbids that overlap outright (`FloodSettings.cs:17-19`).
fn post_import_tags_to_apply(
    config: &FloodConfig,
    request: &PluginDownloadClientMarkImportedRequest,
) -> Vec<String> {
    let scope = post_import_scope_tag(config, request);
    config
        .post_import_tags
        .iter()
        .filter(|tag| {
            !scope
                .as_deref()
                .is_some_and(|scope| tag.trim().eq_ignore_ascii_case(scope))
        })
        .cloned()
        .collect()
}

/// The tag this download was grabbed under.
fn post_import_scope_tag(
    config: &FloodConfig,
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
        .or_else(|| config.tags.first().and_then(|tag| trimmed_non_empty(tag)))
}

/// Sonarr builds an immutable hash set (`Flood.cs:232-234`); Scryer matches
/// case-insensitively and keeps Flood's own casing for the tags it already has
/// (`00-common.md` rule 5).
fn merge_tags(existing: &[String], extra: &[String]) -> Vec<String> {
    let mut tags = existing.to_vec();
    for tag in extra {
        let tag = tag.trim();
        if tag.is_empty()
            || tags
                .iter()
                .any(|held| held.trim().eq_ignore_ascii_case(tag))
        {
            continue;
        }
        tags.push(tag.to_string());
    }
    tags
}

fn same_tag_set(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left.iter().all(|tag| {
            right
                .iter()
                .any(|other| other.trim().eq_ignore_ascii_case(tag.trim()))
        })
}

// ---------------------------------------------------------------------------
// Status and test
// ---------------------------------------------------------------------------

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    respond(status())
}

fn status() -> Result<PluginDownloadClientStatus, PluginError> {
    let config = FloodConfig::from_host();
    let settings: FloodClientSettings =
        serde_json::from_str(&api_call(&config, "GET", "/client/settings", None)?).map_err(
            |error| {
                detailed_error(
                    PluginErrorCode::InvalidConfig,
                    "The configured URL did not answer with Flood client settings; check host, \
                     port and URL base.",
                    error.to_string(),
                )
            },
        )?;
    let root = if config.destination.is_empty() {
        settings.directory_default
    } else {
        config.destination.clone()
    };
    let mut warnings = Vec::new();
    if root.trim().is_empty() {
        warnings.push(
            "Flood reported no default directory. Set Destination so Scryer knows where finished \
             downloads land."
                .to_string(),
        );
    }
    Ok(PluginDownloadClientStatus {
        // Flood's API exposes no version anywhere under `/api`
        // (`server/routes/api/index.ts`): there is no `/version`, and
        // `/auth/verify` answers only `{initialUser, username, level, configs}`
        // with `configs = {authMethod, pollInterval}`. Reporting a guess would
        // be worse than reporting nothing; capability differences between
        // Flood 4.x builds are detected from the response shape instead.
        version: None,
        is_localhost: Some(is_localhost(&config.host)),
        remote_output_roots: if root.trim().is_empty() {
            Vec::new()
        } else {
            vec![root]
        },
        // Post-import tagging never deletes anything, and removal of a finished
        // torrent is the core's decision through the seeding gate.
        removes_completed_downloads: Some(false),
        sorting_mode: Some("flood-api".to_string()),
        warnings,
    })
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    respond(test_connection())
}

fn test_connection() -> Result<String, PluginError> {
    let config = FloodConfig::from_host();
    if let Some(problem) = validate_config(&config) {
        return Err(problem);
    }
    // `Flood.Test` calls `AuthVerify` (`Flood.cs:262`), which in Sonarr always
    // re-runs the login because the proxy is built per request. Drop the cached
    // cookie so a test really exercises the credentials.
    let _ = var::remove(COOKIE_VAR_KEY);
    api_call(&config, "GET", "/auth/verify", None)?;
    Ok("ok".to_string())
}

/// `FloodSettingsValidator` (`FloodSettings.cs:11-21`) in Scryer's shape.
fn validate_config(config: &FloodConfig) -> Option<PluginError> {
    if config.host.trim().is_empty() {
        return Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            "Host is required.",
        ));
    }
    match config.port.trim().parse::<u32>() {
        Ok(port) if (1..=65_535).contains(&port) => {}
        _ => {
            return Some(plugin_error(
                PluginErrorCode::InvalidConfig,
                "Port must be between 1 and 65535.",
            ));
        }
    }
    let overlap: Vec<&str> = config
        .post_import_tags
        .iter()
        .filter(|tag| {
            config
                .tags
                .iter()
                .any(|scope| scope.trim().eq_ignore_ascii_case(tag.trim()))
        })
        .map(|tag| tag.as_str())
        .collect();
    if !overlap.is_empty() {
        return Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "Post Import Tags must not include any tags in the Tags list ({}). A torrent \
                 tagged with both would leave Scryer's scope the moment it is grabbed.",
                overlap.join(", ")
            ),
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

impl FloodConfig {
    fn from_host() -> Self {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "3000".to_string());
        let url_base = config_value("url_base").unwrap_or_default();
        let scheme = if config_bool("use_ssl", false) {
            "https"
        } else {
            "http"
        };
        let authority = if host.contains(':') && !host.starts_with('[') {
            // A bare IPv6 literal has to be bracketed before a port can follow.
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let base = if url_base.trim().is_empty() {
            format!("{scheme}://{authority}")
        } else {
            format!("{scheme}://{authority}/{}", url_base.trim_matches('/'))
        };
        Self {
            api_root: format!("{}/api", base.trim_end_matches('/')),
            host,
            port,
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            destination: config_value("destination").unwrap_or_default(),
            tags: config_list("tags", &["scryer"]),
            post_import_tags: config_list("post_import_tags", &[]),
            additional_tags: config_list("additional_tags", &[]),
            start_on_add: config_bool("start_on_add", true),
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
            Some("Tags added after a successful import. Must not overlap Tags."),
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

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

struct FloodResponse {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

/// One authenticated Flood call, with exactly one re-authentication.
///
/// Sonarr drops the cached cookie on 401/403 and fails the request outright
/// (`FloodProxy.cs:70-75`), so the very next poll pays for a login and the
/// operator sees a spurious "Failed to authenticate". Flood's cookie is a JWT
/// with a one-week expiry (`server/util/authUtil.ts:9`), which means an expired
/// cookie is the *normal* 401, not a wrong password — so retry the login once
/// and only report `AuthFailed` when the fresh credentials are refused too.
fn api_call(
    config: &FloodConfig,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<String, PluginError> {
    let encoded = body
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| {
            detailed_error(
                PluginErrorCode::Permanent,
                "Failed to encode a Flood API request.",
                error.to_string(),
            )
        })?;

    let mut response = send(
        config,
        method,
        path,
        Some(&authenticate(config, false)?),
        encoded.as_deref(),
    )?;
    if matches!(response.status, 401 | 403) && !is_path_access_denied(&response) {
        let _ = var::remove(COOKIE_VAR_KEY);
        response = send(
            config,
            method,
            path,
            Some(&authenticate(config, true)?),
            encoded.as_deref(),
        )?;
    }
    if let Some(error) = classify_response(&response) {
        if matches!(error.code, PluginErrorCode::AuthFailed) {
            let _ = var::remove(COOKIE_VAR_KEY);
        }
        return Err(error);
    }
    Ok(response.body)
}

/// `POST /api/auth/authenticate` (`server/routes/api/auth.ts:70-118`): a JSON
/// `{username, password}` body, a `jwt` httpOnly cookie on success, `400` for
/// a missing field and `401 {"message":"Failed login."}` for bad credentials.
fn authenticate(config: &FloodConfig, force: bool) -> Result<String, PluginError> {
    if !force
        && let Some(cookie) = var::get(COOKIE_VAR_KEY)
            .ok()
            .flatten()
            .map(|value: String| value.trim().to_string())
            .filter(|value| !value.is_empty())
    {
        return Ok(cookie);
    }
    let body = serde_json::to_vec(&serde_json::json!({
        "username": config.username,
        "password": config.password,
    }))
    .map_err(|error| {
        detailed_error(
            PluginErrorCode::Permanent,
            "Failed to encode the Flood login request.",
            error.to_string(),
        )
    })?;
    let response = send(config, "POST", "/auth/authenticate", None, Some(&body))?;
    if let Some(error) = classify_response(&response) {
        return Err(error);
    }
    let cookie = extract_cookie(&response).ok_or_else(|| {
        detailed_error(
            PluginErrorCode::InvalidConfig,
            "Flood accepted the login but returned no session cookie; check that the URL base \
             points at Flood and not at a proxy that strips cookies.",
            truncate(&response.body),
        )
    })?;
    var::set(COOKIE_VAR_KEY, cookie.clone()).map_err(|error| {
        detailed_error(
            PluginErrorCode::Temporary,
            "Failed to store the Flood session cookie.",
            error.to_string(),
        )
    })?;
    Ok(cookie)
}

fn send(
    config: &FloodConfig,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&[u8]>,
) -> Result<FloodResponse, PluginError> {
    let mut request = HttpRequest::new(api_url(config, path))
        .with_method(method)
        .with_header("Accept", "application/json")
        .with_header("Content-Type", "application/json")
        .with_header(
            "User-Agent",
            concat!("scryer-flood-plugin/", env!("CARGO_PKG_VERSION")),
        );
    if let Some(cookie) = cookie {
        request = request.with_header("Cookie", cookie);
    }
    let response = http::request::<Vec<u8>>(&request, body.map(<[u8]>::to_vec))
        .map_err(|error| classify_transport_error(&error.to_string()))?;
    Ok(FloodResponse {
        status: response.status_code(),
        body: String::from_utf8_lossy(&response.body()).to_string(),
        headers: response
            .headers()
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    })
}

/// Flood answers `403 {"code":"EACCES","message":"Permission denied"}` from
/// `add-urls`/`add-files` when the destination is outside its `allowedPaths`
/// (`server/routes/api/torrents.ts:180-182`, `:235-237`;
/// `server/util/fileUtil.ts:8-12`). Sonarr treats every 403 as an
/// authentication failure (`FloodProxy.cs:70-75`); the current API says
/// otherwise, and a re-login cannot fix a denied path.
fn is_path_access_denied(response: &FloodResponse) -> bool {
    response.status == 403 && response.body.contains("EACCES")
}

fn classify_response(response: &FloodResponse) -> Option<PluginError> {
    if is_path_access_denied(response) {
        return Some(detailed_error(
            PluginErrorCode::InvalidConfig,
            "Flood refused the download destination: the path is outside the directories Flood is \
             allowed to write to. Fix Destination, or add the path to Flood's allowedPaths.",
            truncate(&response.body),
        ));
    }
    classify_http_status(
        response.status,
        header_value(response, "location").as_deref(),
        &response.body,
    )
}

fn classify_http_status(status: u16, location: Option<&str>, body: &str) -> Option<PluginError> {
    match status {
        200..=299 => None,
        // The host runs plugin HTTP with `redirect::Policy::none()`, so a login
        // page in front of Flood arrives as a 3xx rather than as unparsable JSON.
        300..=399 => Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            match location.map(str::trim).filter(|value| !value.is_empty()) {
                Some(location) => format!("Flood's API redirected to {location}"),
                None => {
                    "Flood's API redirected the request; check host, port and URL base.".to_string()
                }
            },
        )),
        // `UnauthorizedError` — `FLOOD_UNAUTHORIZED` (`server/errors.ts:3`).
        401 => Some(detailed_error(
            PluginErrorCode::AuthFailed,
            "Failed to authenticate with Flood. Check the username and password.",
            truncate(body),
        )),
        // `AdminRequiredError` — `FLOOD_ADMIN_REQUIRED` (`server/errors.ts:4`).
        403 => Some(detailed_error(
            PluginErrorCode::AuthFailed,
            "Flood refused the request for this user. Check the account's permissions in Flood.",
            truncate(body),
        )),
        404 => Some(detailed_error(
            PluginErrorCode::InvalidConfig,
            "Flood's API was not found at the configured address; check the URL base.",
            truncate(body),
        )),
        // Fastify rejects a body that fails the route's zod schema.
        400 | 422 => Some(detailed_error(
            PluginErrorCode::Permanent,
            "Flood rejected the request as malformed.",
            truncate(body),
        )),
        // `/api/auth/*` is rate limited to 200 requests per 5 minutes
        // (`server/routes/api/auth.ts:63-66`).
        429 => Some(PluginError {
            retry_after_seconds: Some(60),
            ..detailed_error(
                PluginErrorCode::RateLimited,
                "Flood is rate limiting Scryer.",
                truncate(body),
            )
        }),
        500..=599 => Some(detailed_error(
            PluginErrorCode::Temporary,
            format!("Flood returned HTTP {status}."),
            truncate(body),
        )),
        _ => Some(detailed_error(
            PluginErrorCode::Permanent,
            format!("Flood returned HTTP {status}."),
            truncate(body),
        )),
    }
}

/// Sonarr collapses every transport failure into "Unable to connect to Flood,
/// please check your settings" (`FloodProxy.cs:77-82`). Scryer can separate a
/// timeout (retry) from an unreachable host (upstream down).
fn classify_transport_error(detail: &str) -> PluginError {
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("timeout") || lowered.contains("timed out") {
        detailed_error(
            PluginErrorCode::Temporary,
            "Flood did not answer in time.",
            detail,
        )
    } else if lowered.contains("certificate")
        || lowered.contains("tls")
        || lowered.contains("ssl")
        || lowered.contains("trust")
    {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Flood: certificate validation failed.",
            detail,
        )
    } else {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Flood, please check your settings.",
            detail,
        )
    }
}

fn header_value(response: &FloodResponse, name: &str) -> Option<String> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn truncate(value: &str) -> String {
    const LIMIT: usize = 512;
    let trimmed = value.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    trimmed.chars().take(LIMIT).collect::<String>() + "…"
}

fn api_url(config: &FloodConfig, path: &str) -> String {
    format!(
        "{}{}{}",
        config.api_root.trim_end_matches('/'),
        if path.starts_with('/') { "" } else { "/" },
        path
    )
}

/// Flood sets its session as the `jwt` cookie (`server/util/authUtil.ts:20`).
/// Pick that one by name rather than trusting header order — a reverse proxy in
/// front of Flood may add its own.
fn extract_cookie(response: &FloodResponse) -> Option<String> {
    let cookies: Vec<&str> = response
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("set-cookie"))
        .flat_map(|(_, value)| value.split('\n'))
        .collect();
    cookies
        .iter()
        .map(|value| value.split(';').next().unwrap_or_default().trim())
        .find(|value| value.starts_with("jwt="))
        .or_else(|| {
            cookies
                .iter()
                .map(|value| value.split(';').next().unwrap_or_default().trim())
                .find(|value| !value.is_empty())
        })
        .map(ToString::to_string)
}

// ---------------------------------------------------------------------------
// Flood API helpers
// ---------------------------------------------------------------------------

fn list_torrents(config: &FloodConfig) -> Result<HashMap<String, FloodTorrent>, PluginError> {
    let body = api_call(config, "GET", "/torrents", None)?;
    let summary: TorrentListSummary = serde_json::from_str(&body).map_err(|error| {
        detailed_error(
            PluginErrorCode::InvalidConfig,
            "The configured URL did not answer with a Flood torrent list; check host, port and \
             URL base.",
            format!("{error}: {}", truncate(&body)),
        )
    })?;
    Ok(summary.torrents)
}

/// The casing Flood itself uses for a hash.
///
/// Flood keys `/api/torrents` by the **upper-case** info hash — rTorrent's
/// native form, and the qBittorrent gateway upper-cases to match
/// (`server/services/qBittorrent/clientGatewayService.ts:446`). Every action
/// route passes the hash straight through to the backend, and rTorrent's
/// XMLRPC methods are an exact string lookup, so a lower-cased hash silently
/// matches nothing: `POST /torrents/delete` with `deleteData` even throws,
/// because Flood resolves the torrent's directory first
/// (`server/services/rTorrent/clientGatewayService.ts:585-589`). Scryer's own
/// identity stays lower-case (`ClientJobLocator::item_id` is compared verbatim,
/// `crates/scryer-application/src/contracts.rs:287`), so translate at the edge.
fn resolve_client_hash(config: &FloodConfig, hash: &str) -> Result<Option<String>, PluginError> {
    Ok(list_torrents(config)?
        .into_iter()
        .find(|(key, _)| normalize_hash(key) == hash)
        .map(|(key, torrent)| client_hash_of(&key, &torrent)))
}

fn get_contents(config: &FloodConfig, client_hash: &str) -> Result<Vec<String>, PluginError> {
    let body = api_call(
        config,
        "GET",
        &format!("/torrents/{client_hash}/contents"),
        None,
    )?;
    let contents: Vec<TorrentContent> = serde_json::from_str(&body).map_err(|error| {
        detailed_error(
            PluginErrorCode::Temporary,
            "Flood returned an unreadable torrent content list.",
            format!("{error}: {}", truncate(&body)),
        )
    })?;
    Ok(contents
        .into_iter()
        .map(|content| content.path)
        .filter(|path| !path.trim().is_empty())
        .collect())
}

/// Sonarr resolves contents once, at import time (`GetImportItem`,
/// `Flood.cs:190-222`). Scryer's bridge calls `list_completed` on every queue
/// poll, so without a cache each finished torrent costs a request per poll for
/// as long as it seeds. A finished torrent's file list cannot change, so one
/// request per torrent per plugin instance is exactly equivalent.
fn completed_content_paths(
    config: &FloodConfig,
    client_hash: &str,
) -> Result<Vec<String>, PluginError> {
    let key = contents_var_key(client_hash);
    if let Some(cached) = var::get::<Vec<String>>(&key).ok().flatten()
        && !cached.is_empty()
    {
        return Ok(cached);
    }
    let paths = get_contents(config, client_hash)?;
    if !paths.is_empty() {
        let _ = var::set(&key, &paths);
    }
    Ok(paths)
}

fn forget_torrent_state(hash: &str) {
    let _ = var::remove(contents_var_key(hash));
    let _ = var::remove(seed_config_var_key(hash));
}

// ---------------------------------------------------------------------------
// Tagging
// ---------------------------------------------------------------------------

/// `Flood.HandleTags` (`Flood.cs:42-85`) plus Scryer's routed isolation value.
fn tags_for_request(config: &FloodConfig, request: &PluginDownloadClientAddRequest) -> Vec<String> {
    let mut tags = config.tags.clone();
    tags.extend(additional_tags_for_request(config, request));
    if let Some(isolation) = request
        .routing
        .isolation_value
        .as_deref()
        .and_then(trimmed_non_empty)
    {
        tags.push(isolation);
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
    if let Some(value) = value.as_deref().and_then(trimmed_non_empty) {
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

// ---------------------------------------------------------------------------
// Item mapping
// ---------------------------------------------------------------------------

fn torrent_to_item(client_hash: &str, torrent: FloodTorrent, now: i64) -> PluginDownloadItem {
    let hash = normalize_hash(client_hash);
    let remaining = (torrent.size_bytes - torrent.bytes_done).max(0);
    let state = map_state(&torrent);
    let can_remove = derive_can_remove_with_config(&torrent, state, seed_config(&hash), now);
    let message = trimmed_non_empty(&torrent.message);
    PluginDownloadItem {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash.clone()),
        title: torrent.name.clone(),
        state,
        message: message.clone(),
        // Sonarr reports the first tag as the category (`Flood.cs:130`), in
        // Flood's own casing.
        category: torrent.tags.first().cloned(),
        remote_output_path: trimmed_non_empty(&torrent.directory),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(hash),
            client_native_id: Some(client_hash.to_string()),
            tags: torrent.tags.clone(),
            save_path: trimmed_non_empty(&torrent.directory),
            // Deliberately empty: the real content paths cost one request per
            // torrent and are only resolved for finished downloads, where they
            // matter. The download directory is not a content path.
            content_paths: Vec::new(),
            uploaded_bytes: torrent.up_total,
            downloaded_bytes: torrent.down_total.or(Some(torrent.bytes_done)),
            upload_rate_bytes_per_second: torrent.up_rate,
            download_rate_bytes_per_second: torrent.down_rate,
            seed_ratio: Some(torrent.ratio),
            seed_time_seconds: seed_time_seconds(&torrent, now),
            is_private: torrent.is_private,
            raw_status: Some(torrent.status.join(",")),
            status_reason: message,
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.size_bytes),
        remaining_size_bytes: Some(remaining),
        // `-1` is Flood's "infinity" (`shared/types/Torrent.ts`).
        eta_seconds: (torrent.eta > 0).then_some(torrent.eta),
        progress_percent: progress_percent(&torrent),
        // Data completeness only; whether a move is safe while seeding is
        // decided Scryer-side.
        can_move_files: Some(is_completed(&torrent)),
        can_remove,
        removed: Some(false),
        raw_state: Some(torrent.status.join(",")),
        completed_at: completed_at(&torrent),
    }
}

fn torrent_to_completed(
    client_hash: &str,
    torrent: FloodTorrent,
    content_paths: Vec<String>,
) -> PluginCompletedDownload {
    let hash = normalize_hash(client_hash);
    let dest_dir = derive_import_path(&torrent, &content_paths);
    let completed_at = completed_at(&torrent);
    PluginCompletedDownload {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash),
        name: torrent.name.clone(),
        dest_dir: dest_dir.clone(),
        category: torrent.tags.first().cloned(),
        output_kind: Some(if content_paths.len() == 1 {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: content_paths
            .iter()
            .map(|path| join_path(&torrent.directory, path))
            .collect(),
        size_bytes: Some(torrent.size_bytes),
        completed_at,
        parameters: Vec::new(),
        release_name: None,
    }
}

/// `GetImportItem` (`Flood.cs:190-222`): a single-file torrent imports from the
/// file itself, a multi-file torrent from the directory its contents share, and
/// a torrent whose contents diverge at the top level imports from the download
/// directory as it stands.
fn derive_import_path(torrent: &FloodTorrent, content_paths: &[String]) -> String {
    if content_paths.len() == 1 {
        return join_path(&torrent.directory, &content_paths[0]);
    }
    let Some(root) = content_paths.first().and_then(|path| first_segment(path)) else {
        return torrent.directory.clone();
    };
    if content_paths
        .iter()
        .all(|path| first_segment(path) == Some(root))
    {
        join_path(&torrent.directory, root)
    } else {
        torrent.directory.clone()
    }
}

fn first_segment(path: &str) -> Option<&str> {
    path.split(['\\', '/']).find(|segment| !segment.is_empty())
}

fn join_path(directory: &str, path: &str) -> String {
    let directory = directory.trim_end_matches(['/', '\\']);
    let path = path.trim_start_matches(['/', '\\']);
    if directory.is_empty() {
        return path.to_string();
    }
    format!("{directory}/{path}")
}

/// `parse_timestamp` (`crates/scryer-plugins/src/download_client_adapter.rs:311`
/// on `release-0.19.8`, `:289` on `release-next`) accepts either RFC 3339 or a
/// bare Unix-seconds string, so the epoch value Flood reports goes through
/// as-is. Sonarr guards on `DateFinished is > 0` (`Flood.cs:173`) because a
/// never-finished torrent reports `0` — which would otherwise become 1970.
fn completed_at(torrent: &FloodTorrent) -> Option<String> {
    torrent
        .date_finished
        .filter(|finished| *finished > 0)
        .map(|finished| finished.to_string())
}

fn progress_percent(torrent: &FloodTorrent) -> Option<u8> {
    torrent
        .percent_complete
        .filter(|value| value.is_finite())
        .map(|value| value.round().clamp(0.0, 100.0) as u8)
        .or_else(|| {
            (torrent.size_bytes > 0).then(|| {
                ((torrent.bytes_done as f64 / torrent.size_bytes as f64) * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as u8
            })
        })
}

fn has_status(torrent: &FloodTorrent, status: &str) -> bool {
    torrent
        .status
        .iter()
        .any(|value| value.trim().eq_ignore_ascii_case(status))
}

/// Sonarr's table (`Flood.cs:144-159`) against Flood's full status vocabulary
/// (`shared/constants/torrentStatusMap.ts`: `downloading`, `seeding`,
/// `checking`, `complete`, `stopped`, `active`, `inactive`, `warning`, `error`,
/// `moving`). Sonarr tests four of the ten, by substring over a joined string,
/// which also makes `inactive` match `active`. Differences, all deliberate:
///
/// - `checking` is tested first and maps to `Verifying`. Flood's own
///   `hasTorrentFinished` refuses to call a torrent finished while it is
///   checking (`server/util/torrentPropertiesUtil.ts:9-11`), and qBittorrent's
///   `checkingUP` carries `complete` alongside `checking` — importing from a
///   torrent that is being re-hashed is the race this avoids.
/// - `moving` also maps to `Verifying`: Flood 4.14.3 added it for a torrent
///   whose data is being relocated, and Scryer has no `Moving` state. What
///   matters is that the item is busy and must not be imported yet.
/// - `error` is tested before `stopped`. qBittorrent's `error`/`missingFiles`
///   report `['error','inactive','stopped']`, which Sonarr's ordering shows as
///   a merely paused download.
/// - a finished torrent Flood is actively seeding is `Seeding`, which the core
///   maps to the same `Completed` queue state
///   (`crates/scryer-plugins/src/download_client_adapter.rs:354` on
///   `release-0.19.8`).
/// - `warning` is *not* a state: qBittorrent's backend pushes it purely because
///   a tracker sent a message (`getTorrentStatusFromState`), and the message is
///   already carried in `message`/`status_reason`.
/// - anything unrecognised keeps polling as `Downloading` (`00-common.md`
///   rule 2), not `Queued`: an empty status array is a torrent Flood has not
///   classified yet, not one waiting in a queue.
fn map_state(torrent: &FloodTorrent) -> DownloadItemState {
    if has_status(torrent, "checking") || has_status(torrent, "moving") {
        return DownloadItemState::Verifying;
    }
    if has_status(torrent, "complete") || has_status(torrent, "seeding") {
        return if has_status(torrent, "seeding") && !has_status(torrent, "stopped") {
            DownloadItemState::Seeding
        } else {
            DownloadItemState::Completed
        };
    }
    if has_status(torrent, "error") {
        return DownloadItemState::Warning;
    }
    if has_status(torrent, "stopped") {
        return DownloadItemState::Paused;
    }
    DownloadItemState::Downloading
}

fn is_completed(torrent: &FloodTorrent) -> bool {
    !has_status(torrent, "checking")
        && !has_status(torrent, "moving")
        && (has_status(torrent, "complete") || has_status(torrent, "seeding"))
}

/// Honest `can_remove` for Flood.
///
/// Flood exposes no per-torrent seeding limit through its API, so the only goal
/// the plugin can measure is the one Scryer handed it at add time — the same
/// cached `SeedConfiguration` Sonarr reads back through
/// `IDownloadSeedConfigProvider` (`Flood.cs:161-182`). Without that stash the
/// seeding verdict is unknowable and the plugin reports `None`.
fn derive_can_remove_with_config(
    torrent: &FloodTorrent,
    state: DownloadItemState,
    seed_config: Option<FloodSeedConfig>,
    now: i64,
) -> Option<bool> {
    if !matches!(
        state,
        DownloadItemState::Completed | DownloadItemState::Seeding
    ) {
        return Some(false);
    }

    let seed_config = seed_config?;

    if seed_config
        .ratio
        .is_some_and(|ratio| torrent.ratio >= ratio)
    {
        return Some(true);
    }

    if let (Some(finished), Some(seed_time)) = (
        torrent.date_finished.filter(|value| *value > 0),
        seed_config.seed_time_seconds,
    ) {
        return Some(now.saturating_sub(finished) >= seed_time);
    }

    if seed_config.ratio.is_some() {
        Some(false)
    } else {
        None
    }
}

/// Seconds spent seeding, derived from Flood's `dateFinished` timestamp.
fn seed_time_seconds(torrent: &FloodTorrent, now: i64) -> Option<i64> {
    torrent
        .date_finished
        .filter(|finished| *finished > 0)
        .map(|finished| now.saturating_sub(finished).max(0))
}

/// `GetItems` (`Flood.cs:113-122`): a torrent is in scope when it carries every
/// configured tag, and leaves scope once it carries every post-import tag.
///
/// Sonarr's second test is `Settings.PostImportTags.All(...)`, which is
/// vacuously true for an empty list — so with the default configuration (no
/// post-import tags) every torrent is skipped and the queue reads empty.
/// Sonarr's settings model does not guard it either; `PostImportTags` has no
/// constructor default, so the check is only survivable because most operators
/// configure the field. Scryer requires the list to be non-empty before it can
/// exclude anything.
fn matches_scope(config: &FloodConfig, torrent: &FloodTorrent) -> bool {
    let has_tag = |needle: &String| {
        torrent
            .tags
            .iter()
            .any(|tag| tag.trim().eq_ignore_ascii_case(needle.trim()))
    };
    if !config.tags.iter().all(has_tag) {
        return false;
    }
    if config.post_import_tags.is_empty() {
        return true;
    }
    !config.post_import_tags.iter().all(has_tag)
}

// ---------------------------------------------------------------------------
// Info-hash derivation
// ---------------------------------------------------------------------------

/// The release's hash first, then the magnet's `btih` (hex or base32), then
/// SHA-1 of the bencoded `info` dictionary — the three sources Sonarr's core
/// resolves before it ever calls the client (`TorrentClientBase.cs`), because
/// Flood's add routes cannot be relied on to report one.
fn derive_info_hash(request: &PluginDownloadClientAddRequest) -> Option<String> {
    request
        .release
        .info_hash_v1
        .clone()
        .or_else(|| request.release.info_hash_hint.clone())
        .map(|value| normalize_hash(&value))
        .filter(|value| value.len() == 40)
        .or_else(|| {
            [
                request.source.magnet_uri.as_deref(),
                request.source.download_url.as_deref(),
                request.source.torrent_url.as_deref(),
            ]
            .into_iter()
            .flatten()
            .find_map(parse_magnet_info_hash)
        })
        .or_else(|| {
            request
                .source
                .torrent_bytes_base64
                .as_deref()
                .and_then(|value| STANDARD.decode(value).ok())
                .and_then(|bytes| compute_torrent_info_hash(&bytes))
        })
}

fn parse_magnet_info_hash(uri: &str) -> Option<String> {
    let trimmed = uri.trim();
    if !trimmed
        .as_bytes()
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"magnet:?"))
    {
        return None;
    }
    for part in trimmed[8..].split('&') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("xt") && !key.eq_ignore_ascii_case("xt.1") {
            continue;
        }
        let decoded = percent_decode(value);
        if !decoded
            .as_bytes()
            .get(..9)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"urn:btih:"))
        {
            continue;
        }
        if let Some(hash) = normalize_btih(&decoded[9..]) {
            return Some(hash);
        }
    }
    None
}

fn normalize_btih(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() == 40 && value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Some(value.to_ascii_lowercase());
    }
    if value.len() == 32 {
        let decoded = decode_base32(value)?;
        if decoded.len() == 20 {
            return Some(to_lower_hex(&decoded));
        }
    }
    None
}

/// RFC 4648 base32 without padding, the second `btih` encoding a magnet may
/// carry.
fn decode_base32(value: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut pending = 0u32;
    let mut out = Vec::with_capacity(value.len() * 5 / 8);
    for ch in value.chars() {
        if ch == '=' {
            break;
        }
        let index = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32,
            '2'..='7' => ch as u32 - '2' as u32 + 26,
            _ => return None,
        };
        bits = (bits << 5) | index;
        pending += 5;
        if pending >= 8 {
            pending -= 8;
            out.push((bits >> pending) as u8);
        }
    }
    Some(out)
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

/// SHA-1 over the raw bencoded `info` value, byte for byte as it appeared in
/// the file.
fn compute_torrent_info_hash(bytes: &[u8]) -> Option<String> {
    let (start, end) = find_info_dict_range(bytes)?;
    let mut hasher = Sha1::new();
    hasher.update(&bytes[start..end]);
    Some(to_lower_hex(&hasher.finalize()))
}

fn find_info_dict_range(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.first().copied() != Some(b'd') {
        return None;
    }
    let mut idx = 1usize;
    while idx < bytes.len() && bytes[idx] != b'e' {
        let (key, value_start) = parse_bencoded_string(bytes, idx)?;
        let value_end = parse_bencoded_value(bytes, value_start)?;
        if key == b"info" {
            return Some((value_start, value_end));
        }
        idx = value_end;
    }
    None
}

fn parse_bencoded_string(bytes: &[u8], start: usize) -> Option<(&[u8], usize)> {
    let mut idx = start;
    while idx < bytes.len() && bytes[idx] != b':' {
        if !bytes[idx].is_ascii_digit() {
            return None;
        }
        idx += 1;
    }
    if idx >= bytes.len() || idx == start {
        return None;
    }
    let len = std::str::from_utf8(&bytes[start..idx])
        .ok()?
        .parse::<usize>()
        .ok()?;
    let data_start = idx + 1;
    let data_end = data_start.checked_add(len)?;
    if data_end > bytes.len() {
        return None;
    }
    Some((&bytes[data_start..data_end], data_end))
}

fn parse_bencoded_value(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes.get(start)? {
        b'i' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                idx += 1;
            }
            (idx < bytes.len()).then_some(idx + 1)
        }
        b'l' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                idx = parse_bencoded_value(bytes, idx)?;
            }
            (idx < bytes.len()).then_some(idx + 1)
        }
        b'd' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                let (_, next) = parse_bencoded_string(bytes, idx)?;
                idx = parse_bencoded_value(bytes, next)?;
            }
            (idx < bytes.len()).then_some(idx + 1)
        }
        b'0'..=b'9' => parse_bencoded_string(bytes, start).map(|(_, end)| end),
        _ => None,
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

fn nibble_to_hex(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).unwrap_or('0')
}

// ---------------------------------------------------------------------------
// Plugin state
// ---------------------------------------------------------------------------

fn store_seed_config(
    hash: &str,
    request: &PluginDownloadClientAddRequest,
) -> Result<(), PluginError> {
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
        let encoded = serde_json::to_string(&seed_config).map_err(|error| {
            detailed_error(
                PluginErrorCode::Permanent,
                "Failed to encode the seeding goal for this download.",
                error.to_string(),
            )
        })?;
        var::set(seed_config_var_key(hash), encoded).map_err(|error| {
            detailed_error(
                PluginErrorCode::Temporary,
                "Failed to record the seeding goal for this download.",
                error.to_string(),
            )
        })?;
    }

    Ok(())
}

fn seed_config(hash: &str) -> Option<FloodSeedConfig> {
    var::get::<String>(seed_config_var_key(hash))
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn seed_config_var_key(hash: &str) -> String {
    format!("{SEED_CONFIG_VAR_PREFIX}{}", normalize_hash(hash))
}

fn contents_var_key(hash: &str) -> String {
    format!("{CONTENTS_VAR_PREFIX}{}", normalize_hash(hash))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

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
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_ascii_lowercase()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn trimmed_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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

/// `Flood.GetStatus` (`Flood.cs:254`) compares the configured host, not the
/// resolved URL, and counts `127.0.0.1`, `::1` and `localhost`.
fn is_localhost(host: &str) -> bool {
    let host = host.trim().trim_matches(['[', ']']).to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
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
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "title_tags".to_string(),
                label: "Title Tags".to_string(),
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "year".to_string(),
                label: "Year".to_string(),
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "indexer".to_string(),
                label: "Indexer".to_string(),
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "languages".to_string(),
                label: "Language".to_string(),
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "network".to_string(),
                label: "Network".to_string(),
                config_overrides: Default::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn base_config() -> FloodConfig {
        FloodConfig {
            host: "localhost".to_string(),
            port: "3000".to_string(),
            api_root: "http://localhost:3000/api".to_string(),
            username: "user".to_string(),
            password: "secret".to_string(),
            destination: String::new(),
            tags: vec!["scryer".to_string()],
            post_import_tags: Vec::new(),
            additional_tags: Vec::new(),
            start_on_add: true,
        }
    }

    fn torrent(status: &[&str]) -> FloodTorrent {
        FloodTorrent {
            bytes_done: 1_000,
            directory: "/downloads".to_string(),
            name: "Movie".to_string(),
            size_bytes: 1_000,
            status: status.iter().map(|value| (*value).to_string()).collect(),
            tags: vec!["scryer".to_string()],
            ..FloodTorrent::default()
        }
    }

    fn seeding_torrent(ratio: f64) -> FloodTorrent {
        FloodTorrent {
            ratio,
            date_finished: Some(NOW - 600),
            ..torrent(&["complete", "seeding"])
        }
    }

    fn add_request(json: &str) -> PluginDownloadClientAddRequest {
        serde_json::from_str(json).expect("add request fixture")
    }

    fn mark_request(json: &str) -> PluginDownloadClientMarkImportedRequest {
        serde_json::from_str(json).expect("mark-imported request fixture")
    }

    // -----------------------------------------------------------------------
    // Scope filter
    // -----------------------------------------------------------------------

    #[test]
    fn the_default_configuration_does_not_filter_every_torrent_out() {
        // Sonarr's `PostImportTags.All(...)` is vacuously true for an empty
        // list (`Flood.cs:119-122`), which skips every torrent. The regression
        // this guards is an empty queue with a working Flood.
        let config = base_config();
        assert!(config.post_import_tags.is_empty());
        assert!(matches_scope(&config, &torrent(&["downloading"])));
    }

    #[test]
    fn a_torrent_leaves_scope_once_every_post_import_tag_is_present() {
        let config = FloodConfig {
            post_import_tags: vec!["imported".to_string(), "done".to_string()],
            ..base_config()
        };
        let mut item = torrent(&["seeding"]);
        item.tags = vec!["scryer".to_string(), "imported".to_string()];
        assert!(
            matches_scope(&config, &item),
            "one of two tags is not enough"
        );
        item.tags.push("done".to_string());
        assert!(!matches_scope(&config, &item));
    }

    #[test]
    fn scope_tags_match_the_clients_own_casing() {
        let config = FloodConfig {
            tags: vec!["Scryer".to_string()],
            post_import_tags: vec!["Imported".to_string()],
            ..base_config()
        };
        let mut item = torrent(&["seeding"]);
        item.tags = vec!["scryer".to_string()];
        assert!(matches_scope(&config, &item));
        item.tags.push("IMPORTED".to_string());
        assert!(!matches_scope(&config, &item));
    }

    #[test]
    fn a_torrent_missing_a_scope_tag_is_out_of_scope() {
        let config = base_config();
        let mut item = torrent(&["downloading"]);
        item.tags = vec!["other".to_string()];
        assert!(!matches_scope(&config, &item));
    }

    // -----------------------------------------------------------------------
    // Status table
    // -----------------------------------------------------------------------

    #[test]
    fn the_status_table_is_pinned() {
        let cases: &[(&[&str], DownloadItemState)] = &[
            // rTorrent (`server/services/rTorrent/util/torrentPropertiesUtil.ts`)
            (&["checking", "active"], DownloadItemState::Verifying),
            (
                &["complete", "seeding", "active"],
                DownloadItemState::Seeding,
            ),
            (
                &["stopped", "complete", "inactive"],
                DownloadItemState::Completed,
            ),
            (&["downloading", "active"], DownloadItemState::Downloading),
            (&["stopped", "inactive"], DownloadItemState::Paused),
            (
                &["downloading", "error", "active"],
                DownloadItemState::Warning,
            ),
            // qBittorrent (`server/services/qBittorrent/util/torrentPropertiesUtil.ts`)
            (
                &["error", "inactive", "stopped"],
                DownloadItemState::Warning,
            ),
            (
                &["complete", "active", "checking"],
                DownloadItemState::Verifying,
            ),
            (
                &["complete", "inactive", "stopped"],
                DownloadItemState::Completed,
            ),
            (
                &["complete", "inactive", "seeding"],
                DownloadItemState::Seeding,
            ),
            (&["inactive", "downloading"], DownloadItemState::Downloading),
            (&["moving"], DownloadItemState::Verifying),
            (
                &["active", "downloading", "warning"],
                DownloadItemState::Downloading,
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(map_state(&torrent(status)), *expected, "{status:?}");
        }
    }

    #[test]
    fn an_unrecognised_status_keeps_polling_instead_of_warning() {
        // `00-common.md` rule 2: a status vocabulary Scryer has not seen is a
        // newer Flood, not a fault.
        for status in [&["quantum-entangled"][..], &[][..]] {
            assert_eq!(map_state(&torrent(status)), DownloadItemState::Downloading);
        }
    }

    #[test]
    fn inactive_is_not_read_as_active() {
        // Sonarr joins the status array and matches by substring, so `inactive`
        // contains `active`. Element-wise matching is the fix; the observable
        // consequence is only that a status is tested honestly.
        assert!(has_status(&torrent(&["inactive"]), "inactive"));
        assert!(!has_status(&torrent(&["inactive"]), "active"));
    }

    #[test]
    fn a_checking_torrent_is_not_completed_even_when_flood_says_complete() {
        let item = torrent(&["complete", "active", "checking"]);
        assert!(!is_completed(&item));
        assert_eq!(
            torrent_to_item("ABC", item, NOW).can_move_files,
            Some(false)
        );
    }

    // -----------------------------------------------------------------------
    // Seeding verdict
    // -----------------------------------------------------------------------

    #[test]
    fn can_remove_is_false_while_downloading() {
        let item = FloodTorrent {
            bytes_done: 400,
            ..torrent(&["downloading"])
        };
        assert_eq!(
            derive_can_remove_with_config(
                &item,
                map_state(&item),
                Some(FloodSeedConfig {
                    ratio: Some(1.0),
                    seed_time_seconds: None,
                }),
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_ratio_goal() {
        assert_eq!(
            derive_can_remove_with_config(
                &seeding_torrent(0.4),
                DownloadItemState::Seeding,
                Some(FloodSeedConfig {
                    ratio: Some(2.0),
                    seed_time_seconds: None,
                }),
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_true_once_the_ratio_goal_is_met() {
        assert_eq!(
            derive_can_remove_with_config(
                &seeding_torrent(2.5),
                DownloadItemState::Seeding,
                Some(FloodSeedConfig {
                    ratio: Some(2.0),
                    seed_time_seconds: None,
                }),
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_follows_the_seed_time_goal_when_the_finish_time_is_known() {
        let met = derive_can_remove_with_config(
            &seeding_torrent(0.1),
            DownloadItemState::Seeding,
            Some(FloodSeedConfig {
                ratio: None,
                seed_time_seconds: Some(300),
            }),
            NOW,
        );
        assert_eq!(met, Some(true));
        let unmet = derive_can_remove_with_config(
            &seeding_torrent(0.1),
            DownloadItemState::Seeding,
            Some(FloodSeedConfig {
                ratio: None,
                seed_time_seconds: Some(3_600),
            }),
            NOW,
        );
        assert_eq!(unmet, Some(false));
    }

    #[test]
    fn can_remove_is_unknown_without_a_stored_goal() {
        assert_eq!(
            derive_can_remove_with_config(
                &seeding_torrent(9.0),
                DownloadItemState::Seeding,
                None,
                NOW
            ),
            None
        );
    }

    #[test]
    fn can_remove_is_unknown_when_only_a_seed_time_goal_exists_without_a_finish_timestamp() {
        let item = FloodTorrent {
            date_finished: None,
            ..seeding_torrent(0.1)
        };
        assert_eq!(
            derive_can_remove_with_config(
                &item,
                DownloadItemState::Seeding,
                Some(FloodSeedConfig {
                    ratio: None,
                    seed_time_seconds: Some(3_600),
                }),
                NOW
            ),
            None
        );
    }

    #[test]
    fn a_zero_finish_timestamp_is_not_a_finish_timestamp() {
        // `Flood.cs:173` guards on `DateFinished is > 0`.
        let item = FloodTorrent {
            date_finished: Some(0),
            ..seeding_torrent(0.1)
        };
        assert_eq!(completed_at(&item), None);
        assert_eq!(seed_time_seconds(&item, NOW), None);
        assert_eq!(
            derive_can_remove_with_config(
                &item,
                DownloadItemState::Seeding,
                Some(FloodSeedConfig {
                    ratio: None,
                    seed_time_seconds: Some(60),
                }),
                NOW
            ),
            None
        );
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let item = torrent_to_item("ABC", seeding_torrent(0.1), NOW);
        assert_eq!(item.can_move_files, Some(true));
        // No stored goal in the test host, so the seeding verdict is unknown.
        assert_eq!(item.can_remove, None);
    }

    #[test]
    fn seed_time_is_derived_from_date_finished() {
        let item = FloodTorrent {
            date_finished: Some(NOW - 900),
            ..seeding_torrent(0.1)
        };
        assert_eq!(seed_time_seconds(&item, NOW), Some(900));
    }

    // -----------------------------------------------------------------------
    // Item mapping
    // -----------------------------------------------------------------------

    #[test]
    fn completed_at_is_the_unix_seconds_the_core_parses() {
        // `parse_timestamp` accepts RFC 3339 or bare Unix seconds
        // (`download_client_adapter.rs:311` on release-0.19.8).
        let item = torrent_to_item("ABC", seeding_torrent(1.0), NOW);
        assert_eq!(item.completed_at.as_deref(), Some("1699999400"));
    }

    #[test]
    fn scryer_identity_is_lower_case_and_the_clients_own_casing_is_kept_alongside() {
        let item = torrent_to_item("ABCDEF0123", torrent(&["downloading"]), NOW);
        assert_eq!(item.client_item_id, "abcdef0123");
        assert_eq!(item.info_hash.as_deref(), Some("abcdef0123"));
        assert_eq!(
            item.torrent.unwrap().client_native_id.as_deref(),
            Some("ABCDEF0123")
        );
    }

    #[test]
    fn an_infinite_eta_is_not_reported() {
        let item = FloodTorrent {
            eta: -1,
            ..torrent(&["downloading"])
        };
        assert_eq!(torrent_to_item("ABC", item, NOW).eta_seconds, None);
        let item = FloodTorrent {
            eta: 120,
            ..torrent(&["downloading"])
        };
        assert_eq!(torrent_to_item("ABC", item, NOW).eta_seconds, Some(120));
    }

    #[test]
    fn progress_prefers_floods_own_percent_complete() {
        let item = FloodTorrent {
            bytes_done: 0,
            percent_complete: Some(37.4),
            ..torrent(&["downloading"])
        };
        assert_eq!(torrent_to_item("ABC", item, NOW).progress_percent, Some(37));
        let item = FloodTorrent {
            bytes_done: 500,
            size_bytes: 1_000,
            percent_complete: None,
            ..torrent(&["downloading"])
        };
        assert_eq!(torrent_to_item("ABC", item, NOW).progress_percent, Some(50));
    }

    #[test]
    fn rates_and_totals_come_from_floods_own_counters() {
        let parsed: FloodTorrent = serde_json::from_str(
            r#"{"name":"n","status":["downloading"],"upTotal":4096,"downTotal":8192,
                "upRate":10,"downRate":20}"#,
        )
        .unwrap();
        let torrent = torrent_to_item("ABC", parsed, NOW).torrent.unwrap();
        assert_eq!(torrent.uploaded_bytes, Some(4_096));
        assert_eq!(torrent.downloaded_bytes, Some(8_192));
        assert_eq!(torrent.upload_rate_bytes_per_second, Some(10));
        assert_eq!(torrent.download_rate_bytes_per_second, Some(20));
    }

    #[test]
    fn is_private_maps_present_true_present_false_and_absent() {
        let map = |raw: &str| {
            let parsed: FloodTorrent = serde_json::from_str(raw).unwrap();
            torrent_to_item("ABC", parsed, NOW)
                .torrent
                .unwrap()
                .is_private
        };
        assert_eq!(
            map(r#"{"name":"n","status":["seeding"],"isPrivate":true}"#),
            Some(true)
        );
        assert_eq!(
            map(r#"{"name":"n","status":["seeding"],"isPrivate":false}"#),
            Some(false)
        );
        assert_eq!(map(r#"{"name":"n","status":["seeding"]}"#), None);
    }

    #[test]
    fn queue_items_do_not_pass_the_download_directory_off_as_a_content_path() {
        let item = torrent_to_item("ABC", seeding_torrent(1.0), NOW);
        let torrent = item.torrent.unwrap();
        assert_eq!(torrent.save_path.as_deref(), Some("/downloads"));
        assert!(torrent.content_paths.is_empty());
    }

    // -----------------------------------------------------------------------
    // Import paths
    // -----------------------------------------------------------------------

    #[test]
    fn a_single_file_torrent_imports_from_the_file() {
        let paths = vec!["Movie.2020.mkv".to_string()];
        let completed = torrent_to_completed("ABC", seeding_torrent(1.0), paths);
        assert_eq!(completed.dest_dir, "/downloads/Movie.2020.mkv");
        assert_eq!(completed.output_kind, Some(PluginDownloadOutputKind::File));
        assert_eq!(completed.content_paths, vec!["/downloads/Movie.2020.mkv"]);
    }

    #[test]
    fn a_multi_file_torrent_imports_from_the_directory_its_contents_share() {
        let paths = vec![
            "Show.S01/ep1.mkv".to_string(),
            "Show.S01/ep2.mkv".to_string(),
        ];
        let completed = torrent_to_completed("ABC", seeding_torrent(1.0), paths);
        assert_eq!(completed.dest_dir, "/downloads/Show.S01");
        assert_eq!(
            completed.output_kind,
            Some(PluginDownloadOutputKind::Directory)
        );
        assert_eq!(
            completed.content_paths,
            vec!["/downloads/Show.S01/ep1.mkv", "/downloads/Show.S01/ep2.mkv"]
        );
    }

    #[test]
    fn divergent_content_roots_import_from_the_download_directory() {
        let paths = vec!["a/ep1.mkv".to_string(), "b/ep2.mkv".to_string()];
        let completed = torrent_to_completed("ABC", seeding_torrent(1.0), paths);
        assert_eq!(completed.dest_dir, "/downloads");
    }

    #[test]
    fn windows_separators_and_leading_slashes_are_handled() {
        let paths = vec![
            "Show.S01\\ep1.mkv".to_string(),
            "Show.S01\\ep2.mkv".to_string(),
        ];
        let item = FloodTorrent {
            directory: "/downloads/".to_string(),
            ..seeding_torrent(1.0)
        };
        assert_eq!(derive_import_path(&item, &paths), "/downloads/Show.S01");
    }

    // -----------------------------------------------------------------------
    // Tags
    // -----------------------------------------------------------------------

    #[test]
    fn add_tags_union_the_scope_the_additional_tags_and_the_routed_value() {
        let config = FloodConfig {
            tags: vec!["scryer".to_string()],
            additional_tags: vec!["year".to_string(), "indexer".to_string()],
            ..base_config()
        };
        let request = add_request(
            r#"{"source":{"kind":"magnet_uri"},
                "release":{"indexer_name":"Torznab"},
                "title":{"title_name":"Big Buck Bunny","media_facet":"movie","year":2020},
                "routing":{"isolation_value":"tv-scryer"}}"#,
        );
        assert_eq!(
            tags_for_request(&config, &request),
            vec!["scryer", "2020", "Torznab", "tv-scryer"]
        );
    }

    #[test]
    fn add_tags_are_deduplicated_case_insensitively_and_never_empty() {
        let config = FloodConfig {
            tags: vec!["Scryer".to_string(), "scryer".to_string()],
            additional_tags: vec!["network".to_string()],
            ..base_config()
        };
        let request = add_request(
            r#"{"source":{"kind":"magnet_uri"},"release":{},
                "title":{"title_name":"X","media_facet":"tv","network":"   "},
                "routing":{"isolation_value":"  "}}"#,
        );
        assert_eq!(tags_for_request(&config, &request), vec!["Scryer"]);
    }

    #[test]
    fn post_import_tags_union_onto_floods_own_casing() {
        let merged = merge_tags(
            &["Scryer".to_string(), "Imported".to_string()],
            &["imported".to_string(), "done".to_string()],
        );
        assert_eq!(merged, vec!["Scryer", "Imported", "done"]);
    }

    #[test]
    fn an_unchanged_tag_set_is_recognised() {
        assert!(same_tag_set(
            &["Scryer".to_string(), "Imported".to_string()],
            &["imported".to_string(), "scryer".to_string()]
        ));
        assert!(!same_tag_set(
            &["Scryer".to_string()],
            &["Scryer".to_string(), "Imported".to_string()]
        ));
    }

    #[test]
    fn post_import_isolation_is_the_grab_tag_and_is_never_applied_as_a_new_one() {
        // `build_isolation_entries(request.category)` replicates the download's
        // OWN grab tag across every mode
        // (`download_client_adapter.rs:683-700` on release-0.19.8), so treating
        // it as the target would make the handoff a no-op.
        let config = FloodConfig {
            post_import_tags: vec!["imported".to_string()],
            ..base_config()
        };
        let request = mark_request(
            r#"{"client_item_id":"abc","category":"tv-scryer",
                "post_import_isolation":[{"mode":"tag","value":"tv-scryer"}]}"#,
        );
        assert_eq!(
            post_import_scope_tag(&config, &request).as_deref(),
            Some("tv-scryer")
        );
        assert_eq!(
            post_import_tags_to_apply(&config, &request),
            vec!["imported".to_string()]
        );
    }

    #[test]
    fn a_post_import_tag_that_is_the_grab_tag_is_not_reapplied() {
        let config = FloodConfig {
            post_import_tags: vec!["TV-Scryer".to_string()],
            ..base_config()
        };
        let request = mark_request(
            r#"{"client_item_id":"abc",
                "post_import_isolation":[{"mode":"tag","value":"tv-scryer"}]}"#,
        );
        assert!(post_import_tags_to_apply(&config, &request).is_empty());
    }

    #[test]
    fn the_scope_tag_falls_back_to_the_category_then_to_the_configured_tag() {
        let config = base_config();
        let tracked = mark_request(r#"{"client_item_id":"abc","category":"tracked"}"#);
        assert_eq!(
            post_import_scope_tag(&config, &tracked).as_deref(),
            Some("tracked")
        );
        let bare = mark_request(r#"{"client_item_id":"abc"}"#);
        assert_eq!(
            post_import_scope_tag(&config, &bare).as_deref(),
            Some("scryer")
        );
        // An isolation entry whose value is blank is not a scope tag.
        let blank = mark_request(
            r#"{"client_item_id":"abc","post_import_isolation":[{"mode":"tag","value":"  "}]}"#,
        );
        assert_eq!(
            post_import_scope_tag(&config, &blank).as_deref(),
            Some("scryer")
        );
    }

    // -----------------------------------------------------------------------
    // Settings validation
    // -----------------------------------------------------------------------

    #[test]
    fn post_import_tags_must_not_overlap_the_scope_tags() {
        // `FloodSettingsValidator` (`FloodSettings.cs:17-19`).
        let config = FloodConfig {
            tags: vec!["scryer".to_string()],
            post_import_tags: vec!["Scryer".to_string()],
            ..base_config()
        };
        let problem = validate_config(&config).expect("overlap must be rejected");
        assert_eq!(problem.code, PluginErrorCode::InvalidConfig);
        assert!(problem.public_message.contains("Post Import Tags"));
    }

    #[test]
    fn pause_and_resume_map_to_floods_stop_and_start_routes() {
        assert_eq!(
            control_route(DownloadControlAction::Pause),
            Some("/torrents/stop")
        );
        assert_eq!(
            control_route(DownloadControlAction::Resume),
            Some("/torrents/start")
        );
        assert_eq!(control_route(DownloadControlAction::ForceStart), None);
        let raw = scryer_describe(String::new()).expect("descriptor");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(value["provider"]["capabilities"]["pause"], true);
        assert_eq!(value["provider"]["capabilities"]["resume"], true);
        assert_eq!(value["provider"]["capabilities"]["force_start"], false);
    }

    #[test]
    fn a_port_outside_the_valid_range_is_rejected() {
        for port in ["0", "70000", "", "http"] {
            let config = FloodConfig {
                port: port.to_string(),
                ..base_config()
            };
            assert_eq!(
                validate_config(&config).map(|error| error.code),
                Some(PluginErrorCode::InvalidConfig),
                "port {port}"
            );
        }
        assert!(validate_config(&base_config()).is_none());
    }

    // -----------------------------------------------------------------------
    // Errors
    // -----------------------------------------------------------------------

    #[test]
    fn http_status_codes_carry_honest_plugin_error_codes() {
        let cases = [
            (301_u16, PluginErrorCode::InvalidConfig),
            (400, PluginErrorCode::Permanent),
            (401, PluginErrorCode::AuthFailed),
            (403, PluginErrorCode::AuthFailed),
            (404, PluginErrorCode::InvalidConfig),
            (422, PluginErrorCode::Permanent),
            (429, PluginErrorCode::RateLimited),
            (500, PluginErrorCode::Temporary),
            (503, PluginErrorCode::Temporary),
            (418, PluginErrorCode::Permanent),
        ];
        for (status, expected) in cases {
            let error = classify_http_status(status, None, "{}").expect("{status} must fail");
            assert_eq!(error.code, expected, "status {status}");
        }
        assert!(classify_http_status(200, None, "{}").is_none());
        assert!(classify_http_status(202, None, "[]").is_none());
        assert!(classify_http_status(207, None, "[]").is_none());
    }

    #[test]
    fn a_denied_destination_is_a_configuration_problem_not_an_auth_failure() {
        // Flood answers 403 with `EACCES` when the destination is outside
        // `allowedPaths` (`server/routes/api/torrents.ts:180-182`). Sonarr maps
        // every 403 to "Failed to authenticate with Flood"
        // (`FloodProxy.cs:70-75`); a re-login cannot fix a denied path.
        let denied = FloodResponse {
            status: 403,
            body: r#"{"code":"EACCES","message":"Permission denied"}"#.to_string(),
            headers: Vec::new(),
        };
        assert!(is_path_access_denied(&denied));
        assert_eq!(
            classify_response(&denied).map(|error| error.code),
            Some(PluginErrorCode::InvalidConfig)
        );

        let forbidden = FloodResponse {
            status: 403,
            body: r#"{"message":"User is not admin."}"#.to_string(),
            headers: Vec::new(),
        };
        assert!(!is_path_access_denied(&forbidden));
        assert_eq!(
            classify_response(&forbidden).map(|error| error.code),
            Some(PluginErrorCode::AuthFailed)
        );
    }

    #[test]
    fn transport_failures_separate_timeouts_from_unreachable_hosts() {
        assert_eq!(
            classify_transport_error("request timeout").code,
            PluginErrorCode::Temporary
        );
        assert_eq!(
            classify_transport_error("invalid peer certificate").code,
            PluginErrorCode::UpstreamUnavailable
        );
        assert_eq!(
            classify_transport_error("connection refused").code,
            PluginErrorCode::UpstreamUnavailable
        );
    }

    #[test]
    fn the_jwt_cookie_is_picked_by_name() {
        let response = FloodResponse {
            status: 200,
            body: String::new(),
            headers: vec![(
                "set-cookie".to_string(),
                "proxy_session=abc; Path=/\njwt=token-value; Path=/; HttpOnly".to_string(),
            )],
        };
        assert_eq!(
            extract_cookie(&response).as_deref(),
            Some("jwt=token-value")
        );
    }

    // -----------------------------------------------------------------------
    // Info-hash derivation
    // -----------------------------------------------------------------------

    #[test]
    fn the_release_hash_wins_when_scryer_already_has_one() {
        let request = add_request(
            r#"{"source":{"kind":"magnet_uri"},
                "release":{"info_hash_v1":"ABCDEF0123456789ABCDEF0123456789ABCDEF01"},
                "title":{"title_name":"X","media_facet":"tv"},"routing":{}}"#,
        );
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
    }

    #[test]
    fn a_hex_btih_is_read_out_of_the_magnet() {
        let request = add_request(
            r#"{"source":{"kind":"magnet_uri",
                "magnet_uri":"magnet:?xt=urn%3Abtih%3AABCDEF0123456789ABCDEF0123456789ABCDEF01&dn=x"},
                "release":{},"title":{"title_name":"X","media_facet":"tv"},"routing":{}}"#,
        );
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
    }

    #[test]
    fn a_base32_btih_is_decoded_to_hex() {
        let request = add_request(
            r#"{"source":{"kind":"magnet_uri",
                "magnet_uri":"magnet:?xt=urn:btih:AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQT"},
                "release":{},"title":{"title_name":"X","media_facet":"tv"},"routing":{}}"#,
        );
        let hash = derive_info_hash(&request).expect("a base32 btih is a hash");
        assert_eq!(hash.len(), 40);
        assert_eq!(hash, "000102030405060708090a0b0c0d0e0f10111213");
    }

    #[test]
    fn torrent_bytes_are_hashed_over_the_bencoded_info_dictionary() {
        let bytes = b"d8:announce3:foo4:infod4:name3:bar12:piece lengthi16384eee";
        let expected = {
            let mut hasher = Sha1::new();
            hasher.update(b"d4:name3:bar12:piece lengthi16384ee");
            to_lower_hex(&hasher.finalize())
        };
        let encoded = STANDARD.encode(bytes);
        let request = add_request(&format!(
            r#"{{"source":{{"kind":"torrent_file","torrent_bytes_base64":"{encoded}"}},
                "release":{{}},"title":{{"title_name":"X","media_facet":"tv"}},"routing":{{}}}}"#
        ));
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn a_torrent_url_with_no_hash_anywhere_stays_underivable() {
        let request = add_request(
            r#"{"source":{"kind":"torrent_url","torrent_url":"https://indexer/dl?id=1"},
                "release":{},"title":{"title_name":"X","media_facet":"tv"},"routing":{}}"#,
        );
        assert_eq!(derive_info_hash(&request), None);
    }

    #[test]
    fn floods_own_add_response_hash_is_preferred_when_it_reports_one() {
        // `hashesResponseSchema` on 200/202/207
        // (`server/routes/api/torrents.ts:104`, `:158-168`). The rTorrent
        // gateway answers `[]`, which must not be read as a hash.
        assert_eq!(
            reported_add_hash(r#"["ABCDEF0123456789ABCDEF0123456789ABCDEF01"]"#).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
        assert_eq!(reported_add_hash("[]"), None);
        assert_eq!(reported_add_hash("{}"), None);
        assert_eq!(reported_add_hash(""), None);
    }

    // -----------------------------------------------------------------------
    // Misc
    // -----------------------------------------------------------------------

    #[test]
    fn localhost_covers_the_three_names_sonarr_accepts() {
        for host in ["localhost", "127.0.0.1", "::1", "[::1]", " LOCALHOST "] {
            assert!(is_localhost(host), "{host}");
        }
        for host in ["flood.example.com", "10.0.0.5", ""] {
            assert!(!is_localhost(host), "{host}");
        }
    }

    #[test]
    fn a_bare_ipv6_host_is_bracketed_in_the_api_url() {
        let config = FloodConfig {
            host: "::1".to_string(),
            api_root: "http://[::1]:3000/api".to_string(),
            ..base_config()
        };
        assert_eq!(
            api_url(&config, "/torrents"),
            "http://[::1]:3000/api/torrents"
        );
    }

    #[test]
    fn floods_own_hash_casing_is_what_goes_back_on_the_wire() {
        // rTorrent's XMLRPC lookup is an exact string match on the upper-case
        // hash, and Flood's qBittorrent gateway upper-cases to match
        // (`server/services/qBittorrent/clientGatewayService.ts:446`). A
        // lower-cased hash matches nothing, and
        // `POST /torrents/delete {deleteData:true}` even throws because Flood
        // resolves the torrent's directory first
        // (`server/services/rTorrent/clientGatewayService.ts:585-589`).
        let mut item = torrent(&["seeding"]);
        item.hash = Some("ABCDEF01".to_string());
        assert_eq!(client_hash_of("abcdef01", &item), "ABCDEF01");
        item.hash = None;
        assert_eq!(client_hash_of("ABCDEF01", &item), "ABCDEF01");
        item.hash = Some("   ".to_string());
        assert_eq!(client_hash_of("ABCDEF01", &item), "ABCDEF01");
    }

    #[test]
    fn state_keys_are_normalised_so_floods_casing_never_forks_the_stash() {
        assert_eq!(seed_config_var_key("ABCDEF"), seed_config_var_key("abcdef"));
        assert_eq!(contents_var_key("ABCDEF"), contents_var_key("abcdef"));
    }

    #[test]
    fn the_descriptor_declares_the_non_destructive_handoff_it_implements() {
        let descriptor: serde_json::Value =
            serde_json::from_str(&scryer_describe(String::new()).unwrap()).unwrap();
        assert_eq!(
            descriptor["provider"]["capabilities"]["mark_imported_non_destructive"],
            serde_json::Value::Bool(true)
        );
        assert!(functions().mark_imported_non_destructive.is_some());
    }

    #[test]
    fn the_client_never_removes_a_finished_download_on_its_own() {
        let descriptor: serde_json::Value =
            serde_json::from_str(&scryer_describe(String::new()).unwrap()).unwrap();
        assert_eq!(
            descriptor["provider"]["capabilities"]["mark_imported"],
            serde_json::Value::Bool(true)
        );
        // `post_import_action` never existed on this client, so there is no
        // legacy remove option to migrate; the status contract is the guard.
        assert!(
            !config_fields()
                .iter()
                .any(|field| field.key == "post_import_action")
        );
    }
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
