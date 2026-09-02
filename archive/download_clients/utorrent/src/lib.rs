use base64::{Engine as _, engine::general_purpose::STANDARD};
use roxmltree::Document;
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
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

const TOKEN_VAR_KEY: &str = "utorrent.token";
const COOKIE_VAR_KEY: &str = "utorrent.cookie";
/// One state slot for the differential `list=1` cache. Sonarr keys its cache by
/// `host:port:category` (`UTorrent.cs:187`); the state service is a flat map per
/// client instance, so the composite key travels inside the value and a key
/// mismatch is a miss.
const TORRENT_CACHE_VAR_KEY: &str = "utorrent.torrent_cache";

const STATUS_STARTED: i64 = 1;
const STATUS_CHECKED: i64 = 8;
const STATUS_ERROR: i64 = 16;
const STATUS_PAUSED: i64 = 32;
const STATUS_QUEUED: i64 = 64;
const STATUS_LOADED: i64 = 128;

/// uTorrent 3.0. Below this build `list=1` rows stop after the `remaining`
/// column, so neither the status message nor the root download path exists
/// (`UTorrentTorrent.cs:80`, `UTorrent.cs:273`).
const MIN_SUPPORTED_BUILD: i64 = 25406;

/// Sonarr keeps its differential torrent cache for 15 minutes
/// (`UTorrent.cs:212`). The state service has no expiry of its own, so the
/// entry carries its own timestamp.
const TORRENT_CACHE_TTL_SECONDS: i64 = 15 * 60;

/// The host caps a plugin's whole state map at 1 MiB
/// (`crates/scryer-plugins/src/wasmtime_host/command_host.rs:31`), shared with
/// the token and cookie. A queue large enough to approach that budget skips the
/// cache and keeps polling in full rather than failing the poll.
const MAX_TORRENT_CACHE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
struct UTorrentConfig {
    host: String,
    port: String,
    gui_url: String,
    username: String,
    password: String,
    category: String,
    post_import_category: String,
    recent_priority_first: bool,
    older_priority_first: bool,
    initial_state: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// `list=1` index 24 ("date completed", unix seconds) on builds that report it; `0` when
    /// the torrent never completed or the build predates the column.
    date_completed: i64,
}

#[derive(Default, Deserialize)]
struct UTorrentResponse {
    #[serde(default)]
    build: i64,
    /// Absent on a differential answer, which is exactly how Sonarr decides to
    /// merge instead of replace (`UTorrent.cs:192`).
    #[serde(default)]
    torrents: Option<Vec<Vec<serde_json::Value>>>,
    #[serde(default, rename = "torrentp")]
    torrents_changed: Vec<Vec<serde_json::Value>>,
    #[serde(default, rename = "torrentm")]
    torrents_removed: Vec<String>,
    #[serde(default, rename = "torrentc")]
    cache_number: Option<String>,
    #[serde(default)]
    settings: Vec<Vec<serde_json::Value>>,
}

/// The cached side of Sonarr's `UTorrentTorrentCache` (`UTorrentTorrentCache.cs`).
#[derive(Serialize, Deserialize)]
struct TorrentCache {
    key: String,
    cache_id: String,
    stored_at: i64,
    torrents: Vec<UTorrentTorrent>,
}

struct RawResponse {
    body_text: String,
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// `Err(Error::msg(..))` reaches the host as `PluginErrorCode::Temporary`, so
/// every failure this plugin can name carries its own code instead
/// (`00-common.md` rule 4). The distinctions mirror Sonarr's exception classes
/// in `UTorrentProxy.cs:204-293` and its validation failures in
/// `UTorrent.cs:267-330`.
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
            "Scryer sent a request this uTorrent plugin could not read.",
            error.to_string(),
        )
    })
}

fn truncate(value: &str) -> String {
    const LIMIT: usize = 512;
    if value.len() <= LIMIT {
        return value.to_string();
    }
    let mut end = LIMIT;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// The host runs plugin HTTP with redirects disabled, so a reverse proxy that
/// bounces the Web UI to a login page arrives as a 3xx rather than as an
/// unparseable body — Sonarr only ever sees that as a deserialization failure.
fn classify_http_status(status: u16, location: Option<&str>, body: &str) -> Option<PluginError> {
    match status {
        200..=299 => None,
        300..=399 => Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            match location.map(str::trim).filter(|value| !value.is_empty()) {
                Some(location) => {
                    format!("uTorrent's Web UI redirected to {location}; check the URL base.")
                }
                None => "uTorrent's Web UI redirected the request; check host, port and URL base."
                    .to_string(),
            },
        )),
        401 | 403 => Some(plugin_error(
            PluginErrorCode::AuthFailed,
            "Failed to authenticate with uTorrent.",
        )),
        404 => Some(plugin_error(
            PluginErrorCode::InvalidConfig,
            "uTorrent's Web UI was not found at this address; check the URL base.",
        )),
        429 => Some(PluginError {
            retry_after_seconds: Some(60),
            ..plugin_error(
                PluginErrorCode::Temporary,
                "uTorrent is rate limiting Scryer.",
            )
        }),
        500..=599 => Some(detailed_error(
            PluginErrorCode::Temporary,
            format!("uTorrent returned HTTP {status}."),
            truncate(body),
        )),
        _ => Some(detailed_error(
            PluginErrorCode::Permanent,
            format!("uTorrent returned HTTP {status}."),
            truncate(body),
        )),
    }
}

/// The host hands transport failures back as a string, so classification is by
/// substring. This is the closest this surface gets to Sonarr's
/// `WebExceptionStatus.TrustFailure` / `ConnectFailure` split
/// (`UTorrentProxy.cs:232-240`, `UTorrent.cs:291-303`).
fn classify_transport_error(detail: &str) -> PluginError {
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("timeout") || lowered.contains("timed out") {
        detailed_error(
            PluginErrorCode::Temporary,
            "uTorrent did not answer in time.",
            detail,
        )
    } else if lowered.contains("certificate")
        || lowered.contains("tls")
        || lowered.contains("ssl")
        || lowered.contains("trust")
    {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to uTorrent: certificate validation failed.",
            detail,
        )
    } else {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to uTorrent, please check your settings.",
            detail,
        )
    }
}

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
                    // Sonarr's `PreferTorrentFile` is `false` for uTorrent
                    // (`TorrentClientBase.cs:50`), so the magnet leads; a
                    // core-supplied `.torrent` body comes next because a
                    // plugin-side GET of a torrent URL carries no indexer
                    // cookies.
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
                    supports_stopped: true,
                    supports_force_start: true,
                    supports_queue_placement: true,
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
                // The core only calls the non-destructive handoff, and only for
                // a client that advertises it
                // (`download_client_adapter.rs:1460-1476`).
                mark_imported_non_destructive: true,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

pub fn scryer_download_add(input: String) -> FnResult<String> {
    respond(add(&input))
}

fn add(input: &str) -> Result<PluginDownloadClientAddResponse, PluginError> {
    let request: PluginDownloadClientAddRequest = parse_request(input)?;
    let config = UTorrentConfig::from_host();

    // uTorrent's `add-url` and `add-file` answer with nothing but the build
    // number, so the hash Scryer tracks the download by has to be known before
    // the add. Sonarr computes it in `TorrentClientBase` (`:208` and `:233`)
    // and hands it down; here the release's hash leads and the source is the
    // fallback.
    let hash = derive_info_hash(&request).ok_or_else(|| {
        plugin_error(
            PluginErrorCode::Permanent,
            "uTorrent cannot report the hash of a torrent it was handed, and this release carries no info hash, magnet link or torrent file to derive one from.",
        )
    })?;

    match add_payload(&request) {
        Some(AddPayload::Url(source)) => {
            request_json(
                &config,
                &[
                    ("action".to_string(), "add-url".to_string()),
                    ("s".to_string(), source),
                ],
                "GET",
                None,
                None,
            )?;
        }
        Some(AddPayload::Bytes(encoded)) => {
            let decoded = STANDARD.decode(encoded).map_err(|error| {
                detailed_error(
                    PluginErrorCode::Permanent,
                    "Scryer supplied a torrent body that is not valid base64.",
                    error.to_string(),
                )
            })?;
            let filename = torrent_file_name(&request, &hash);
            post_multipart(
                &config,
                &[
                    ("action".to_string(), "add-file".to_string()),
                    ("path".to_string(), String::new()),
                ],
                "torrent_file",
                &filename,
                &decoded,
            )?;
        }
        None => {
            return Err(plugin_error(
                PluginErrorCode::Permanent,
                "download source is missing",
            ));
        }
    }

    set_seed_config(&config, &hash, &request)?;
    if !config.category.is_empty() {
        set_label(&config, &hash, &config.category)?;
    }
    if let Some(action) = queue_action(&config, &request) {
        request_json(
            &config,
            &[
                ("action".to_string(), action.to_string()),
                ("hash".to_string(), hash.clone()),
            ],
            "GET",
            None,
            None,
        )?;
    }
    if let Some(action) = initial_state_action(&config, &request) {
        request_json(
            &config,
            &[
                ("action".to_string(), action),
                ("hash".to_string(), hash.clone()),
            ],
            "GET",
            None,
            None,
        )?;
    }

    Ok(PluginDownloadClientAddResponse {
        client_item_id: hash.clone(),
        info_hash: Some(hash),
    })
}

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    respond(list_queue())
}

fn list_queue() -> Result<Vec<PluginDownloadItem>, PluginError> {
    let config = UTorrentConfig::from_host();
    Ok(list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent.label == config.category)
        .map(torrent_to_item)
        .collect())
}

/// uTorrent keeps no failed history: `list=1` is the whole world, and a torrent
/// that failed is still in it carrying `UTorrentTorrentStatus.Error`
/// (`UTorrent.cs:148`). The bridge only keeps `Failed`/`Error` items out of
/// this call (`download_client_bridge.rs:188`), so answering with the same list
/// a second time would cost one extra `list=1` per poll and change nothing.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    respond(Ok::<Vec<PluginDownloadItem>, PluginError>(Vec::new()))
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    respond(list_completed())
}

fn list_completed() -> Result<Vec<PluginCompletedDownload>, PluginError> {
    let config = UTorrentConfig::from_host();
    Ok(list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent.label == config.category)
        .filter(is_completed)
        .map(torrent_to_completed)
        .collect())
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    respond(control(&input))
}

fn control(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientControlRequest = parse_request(input)?;
    let config = UTorrentConfig::from_host();
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
    request_json(
        &config,
        &[
            ("action".to_string(), action.to_string()),
            ("hash".to_string(), normalize_hash(&request.client_item_id)),
        ],
        "GET",
        None,
        None,
    )?;
    Ok(())
}

/// Kept as an alias of the non-destructive handoff.
///
/// The core has no caller for the destructive variant
/// (`import/completed_download/result_state.rs::schedule_non_destructive_import_mark`),
/// and uTorrent's post-import step never deletes anything anyway, so both
/// entry points run the same body.
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    respond(mark_imported(&input))
}

pub fn scryer_download_mark_imported_non_destructive(input: String) -> FnResult<String> {
    respond(mark_imported(&input))
}

fn mark_imported(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientMarkImportedRequest = parse_request(input)?;
    let config = UTorrentConfig::from_host();
    let hash = normalize_hash(
        &request
            .info_hash
            .clone()
            .unwrap_or_else(|| request.client_item_id.clone()),
    );
    if hash.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ));
    }

    let Some(plan) = post_import_plan(&config, &request) else {
        return Ok(());
    };

    // Sonarr sets the imported label first and then runs the removal trick on
    // the grab label (`UTorrent.cs:50-56`). That order relies on uTorrent 3.3+
    // multi-label behaviour; on a single-label build (3.0-3.2 are still above
    // this plugin's version floor) `setprops` applies its `s`/`v` pairs in
    // order and the sequence ends with an empty label. Removing first and then
    // applying the imported label yields the same end state on both kinds of
    // build: the grab label leaves the label list and the torrent carries the
    // imported label.
    if let Some(scope) = plan.scope_label.as_deref() {
        remove_label(&config, &hash, scope)?;
    }
    set_label(&config, &hash, &plan.imported_label)?;
    Ok(())
}

/// What the post-import handoff should do, or `None` for "nothing to do".
#[derive(Debug, PartialEq, Eq)]
struct PostImportPlan {
    imported_label: String,
    /// The label this download was grabbed under, which the imported label
    /// replaces. `None` when it is not known or already equals the target.
    scope_label: Option<String>,
}

/// The core fills `post_import_isolation` with the download's *own* grab
/// category replicated across every isolation mode
/// (`crates/scryer-plugins/src/download_client_adapter.rs:657-674`, called at
/// `:1476`). It is therefore the label to drop, never the label to apply — the
/// label to apply is always the configured post-import category, exactly as in
/// Sonarr's `Settings.TvImportedCategory` (`UTorrent.cs:47`).
fn post_import_plan(
    config: &UTorrentConfig,
    request: &PluginDownloadClientMarkImportedRequest,
) -> Option<PostImportPlan> {
    let imported_label = non_empty(config.post_import_category.clone())?;
    let scope_label = request
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
        .or_else(|| non_empty(request.category.clone().unwrap_or_default()))
        .or_else(|| non_empty(config.category.clone()));

    // Sonarr's `TvImportedCategory != TvCategory` guard (`UTorrent.cs:48`).
    if scope_label
        .as_deref()
        .is_some_and(|scope| scope.eq_ignore_ascii_case(&imported_label))
    {
        return None;
    }

    Some(PostImportPlan {
        imported_label,
        scope_label,
    })
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    respond(status())
}

fn status() -> Result<PluginDownloadClientStatus, PluginError> {
    let config = UTorrentConfig::from_host();
    let response = get_settings(&config)?;
    let root = output_root(&settings_map(&response), &config.category);
    Ok(PluginDownloadClientStatus {
        version: Some(response.build.to_string()),
        is_localhost: Some(is_localhost_host(&config.host)),
        remote_output_roots: if root.is_empty() {
            Vec::new()
        } else {
            vec![root]
        },
        // Removal of a finished torrent is the core's decision through the
        // seeding gate; the post-import step only relabels.
        removes_completed_downloads: Some(false),
        sorting_mode: Some("utorrent-webui".to_string()),
        warnings: vec![
            "uTorrent has shipped bundled cryptominers, malware and ads; consider a different torrent client.".to_string(),
        ],
    })
}

/// `UTorrent.cs:226-241`: the active-download directory only counts while its
/// flag is set, the completed-download directory overrides it, and the label
/// subdirectory is appended only when uTorrent is configured to add one.
fn output_root(settings: &std::collections::HashMap<String, String>, category: &str) -> String {
    let flag = |key: &str| settings.get(key).is_some_and(|value| value == "true");
    let mut root = String::new();
    if flag("dir_active_download_flag") {
        root = settings
            .get("dir_active_download")
            .cloned()
            .unwrap_or_default();
    }
    if flag("dir_completed_download_flag") {
        root = settings
            .get("dir_completed_download")
            .cloned()
            .unwrap_or_default();
        if flag("dir_add_label") && !category.is_empty() {
            root = join_path(&root, category);
        }
    }
    root
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    respond(test_connection())
}

fn test_connection() -> Result<String, PluginError> {
    let config = UTorrentConfig::from_host();
    let _ = var::remove(TOKEN_VAR_KEY);
    let _ = var::remove(COOKIE_VAR_KEY);
    let _ = var::remove(TORRENT_CACHE_VAR_KEY);
    let response = get_settings(&config)?;
    if response.build < MIN_SUPPORTED_BUILD {
        // Sonarr's `DownloadClientValidationErrorVersion`: a client too old to
        // answer correctly is a configuration problem, not a transient one.
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "uTorrent version should be at least 3.0 (build {MIN_SUPPORTED_BUILD}). Version reported is {}.",
                response.build
            ),
        ));
    }
    // Sonarr's second validation step (`UTorrent.cs:317-329`).
    list_torrents(&config).map_err(|error| PluginError {
        public_message: format!(
            "Failed to get the list of torrents: {}",
            error.public_message
        ),
        ..error
    })?;
    Ok(response.build.to_string())
}

impl UTorrentConfig {
    fn from_host() -> Self {
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
        Self {
            gui_url: format!("{}/gui/", base.trim_end_matches('/')),
            host,
            port,
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            category: config_value("category").unwrap_or_else(|| "scryer-tv".to_string()),
            post_import_category: config_value("post_import_category").unwrap_or_default(),
            recent_priority_first: config_value("recent_priority").as_deref() == Some("first"),
            older_priority_first: config_value("older_priority").as_deref() == Some("first"),
            initial_state: config_value("initial_state").unwrap_or_else(|| "start".to_string()),
        }
    }

    fn cache_key(&self) -> String {
        format!("{}:{}:{}", self.host, self.port, self.category)
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
            Some("scryer-tv"),
            None,
        ),
        field(
            "post_import_category",
            "Post Import Category",
            ConfigFieldType::String,
            false,
            None,
            Some("uTorrent label applied once Scryer has imported the download. The grab label is replaced; nothing is removed from disk."),
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
                    config_overrides: Default::default(),
                },
                ConfigFieldOption {
                    value: "forcestart".to_string(),
                    label: "Force Start".to_string(),
                    config_overrides: Default::default(),
                },
                ConfigFieldOption {
                    value: "pause".to_string(),
                    label: "Pause".to_string(),
                    config_overrides: Default::default(),
                },
                ConfigFieldOption {
                    value: "stop".to_string(),
                    label: "Stop".to_string(),
                    config_overrides: Default::default(),
                },
            ],
            help_text: Some(
                "Applied after adding a torrent, unless Scryer routes an explicit state for that grab."
                    .to_string(),
            ),
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
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "first".to_string(),
                label: "First".to_string(),
                config_overrides: Default::default(),
            },
        ],
        help_text: None,
    }
}

// ---------------------------------------------------------------------------
// Add-time decisions
// ---------------------------------------------------------------------------

/// What uTorrent is handed: a URL it fetches itself (`add-url`, which also
/// takes magnets) or a `.torrent` body Scryer already holds (`add-file`).
#[derive(Debug, PartialEq, Eq)]
enum AddPayload {
    Url(String),
    Bytes(String),
}

/// Honour the source kind the core selected from `preferred_sources`, and fall
/// back to whatever else the request carries rather than failing an add that
/// could have succeeded.
fn add_payload(request: &PluginDownloadClientAddRequest) -> Option<AddPayload> {
    let magnet = non_empty(request.source.magnet_uri.clone().unwrap_or_default())
        .or_else(|| magnet_from(request.source.download_url.as_deref()))
        .map(AddPayload::Url);
    let bytes = non_empty(
        request
            .source
            .torrent_bytes_base64
            .clone()
            .unwrap_or_default(),
    )
    .map(AddPayload::Bytes);
    let url = non_empty(request.source.torrent_url.clone().unwrap_or_default())
        .or_else(|| non_empty(request.source.download_url.clone().unwrap_or_default()))
        .map(AddPayload::Url);

    let ordered = match request.source.kind {
        DownloadInputKind::MagnetUri => [magnet, bytes, url],
        DownloadInputKind::TorrentUrl => [url, bytes, magnet],
        DownloadInputKind::TorrentBytes | DownloadInputKind::TorrentFile => [bytes, magnet, url],
        DownloadInputKind::Nzb | DownloadInputKind::NzbUrl => return None,
    };
    ordered.into_iter().flatten().next()
}

fn magnet_from(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| {
            value
                .as_bytes()
                .get(..7)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"magnet:"))
        })
        .map(str::to_string)
}

fn torrent_file_name(request: &PluginDownloadClientAddRequest, hash: &str) -> String {
    if let Some(name) = non_empty(request.source.torrent_file_name.clone().unwrap_or_default()) {
        return name;
    }
    match non_empty(request.release.release_title.clone().unwrap_or_default()) {
        Some(title) => format!("{}.torrent", clean_file_name(&title)),
        None => format!("{hash}.torrent"),
    }
}

fn clean_file_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// `UTorrent.cs:78` always sends `SetState(hash, Settings.IntialState)`. Scryer
/// additionally lets the core route a state for one grab, and that wins over
/// the configured default.
fn initial_state_action(
    config: &UTorrentConfig,
    request: &PluginDownloadClientAddRequest,
) -> Option<String> {
    let torrent = request.torrent.as_ref();
    let force_start = torrent.and_then(|torrent| torrent.force_start) == Some(true);
    match torrent.and_then(|torrent| torrent.initial_state) {
        Some(PluginTorrentInitialState::Paused) => Some("pause".to_string()),
        Some(PluginTorrentInitialState::Stopped) => Some("stop".to_string()),
        Some(PluginTorrentInitialState::Started) => Some(if force_start {
            "forcestart".to_string()
        } else {
            "start".to_string()
        }),
        Some(PluginTorrentInitialState::Default) | None => {
            if force_start {
                Some("forcestart".to_string())
            } else {
                non_empty(config.initial_state.clone())
            }
        }
    }
}

/// `UTorrent.cs:72-76`: recent/older priority only ever moves a torrent to the
/// top. An explicit `queue_placement` from the core wins, and uTorrent can also
/// honour the bottom half of that contract.
fn queue_action(
    config: &UTorrentConfig,
    request: &PluginDownloadClientAddRequest,
) -> Option<&'static str> {
    match request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.queue_placement)
    {
        Some(PluginTorrentQueuePlacement::First) => Some("queuetop"),
        Some(PluginTorrentQueuePlacement::Last) => Some("queuebottom"),
        Some(PluginTorrentQueuePlacement::Default) | None => {
            let recent = request.release.is_recent.unwrap_or(false);
            ((recent && config.recent_priority_first) || (!recent && config.older_priority_first))
                .then_some("queuetop")
        }
    }
}

// ---------------------------------------------------------------------------
// Info-hash derivation
// ---------------------------------------------------------------------------

/// The release's hash first, then the magnet's `btih` (hex or base32), then
/// SHA-1 of the bencoded `info` dictionary — Sonarr's
/// `MagnetLink.Parse(..).InfoHashes.V1OrV2.ToHex()` (`TorrentClientBase.cs:233`)
/// and `ITorrentFileInfoReader.GetHashFromTorrentFile` (`:208`).
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

/// RFC 4648 base32 without padding, which is the second `btih` encoding
/// `MagnetLink.Parse` accepts (fixture `Download_should_get_hash_from_magnet_url`).
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
/// the file — the definition `GetHashFromTorrentFile` implements.
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
// Transport
// ---------------------------------------------------------------------------

fn get_settings(config: &UTorrentConfig) -> Result<UTorrentResponse, PluginError> {
    request_json(
        config,
        &[("action".to_string(), "getsettings".to_string())],
        "GET",
        None,
        None,
    )
}

/// Sonarr's differential poll (`UTorrent.cs:183-215` + `UTorrentProxy.cs:73-86`):
/// pass the last `torrentc` as `cid`, and when uTorrent answers with only
/// `torrentp`/`torrentm`, merge them into the cached list instead of asking for
/// everything again.
fn list_torrents(config: &UTorrentConfig) -> Result<Vec<UTorrentTorrent>, PluginError> {
    let cache_key = config.cache_key();
    let cached = load_torrent_cache(&cache_key, now_unix_seconds());
    let mut params = vec![("list".to_string(), "1".to_string())];
    if let Some(cache) = cached.as_ref() {
        params.push(("cid".to_string(), cache.cache_id.clone()));
    }
    let response = request_json(config, &params, "GET", None, None)?;
    let torrents = merge_torrents(cached.as_ref(), &response);
    store_torrent_cache(&cache_key, response.cache_number.as_deref(), &torrents);
    Ok(torrents)
}

fn merge_torrents(
    cache: Option<&TorrentCache>,
    response: &UTorrentResponse,
) -> Vec<UTorrentTorrent> {
    let changed: Vec<UTorrentTorrent> = response
        .torrents_changed
        .iter()
        .cloned()
        .map(map_torrent)
        .collect();
    match (cache, response.torrents.as_ref()) {
        (Some(cache), None) => {
            let mut superseded: Vec<String> =
                changed.iter().map(|torrent| torrent.hash.clone()).collect();
            superseded.extend(
                response
                    .torrents_removed
                    .iter()
                    .map(|hash| normalize_hash(hash)),
            );
            cache
                .torrents
                .iter()
                .filter(|torrent| !superseded.contains(&torrent.hash))
                .cloned()
                .chain(changed)
                .collect()
        }
        (_, Some(rows)) => rows.iter().cloned().map(map_torrent).collect(),
        (None, None) => changed,
    }
}

fn load_torrent_cache(key: &str, now: i64) -> Option<TorrentCache> {
    let raw: String = var::get(TORRENT_CACHE_VAR_KEY).ok().flatten()?;
    let cache: TorrentCache = serde_json::from_str(&raw).ok()?;
    cache_is_usable(&cache, key, now).then_some(cache)
}

/// Sonarr's cache is keyed `host:port:category` and expires after 15 minutes
/// (`UTorrent.cs:187`, `:212`). The state service has neither, so both rules
/// live here.
fn cache_is_usable(cache: &TorrentCache, key: &str, now: i64) -> bool {
    cache.key == key
        && !cache.cache_id.is_empty()
        && now.saturating_sub(cache.stored_at) < TORRENT_CACHE_TTL_SECONDS
}

fn store_torrent_cache(key: &str, cache_id: Option<&str>, torrents: &[UTorrentTorrent]) {
    let Some(cache_id) = cache_id.map(str::trim).filter(|value| !value.is_empty()) else {
        let _ = var::remove(TORRENT_CACHE_VAR_KEY);
        return;
    };
    let cache = TorrentCache {
        key: key.to_string(),
        cache_id: cache_id.to_string(),
        stored_at: now_unix_seconds(),
        torrents: torrents.to_vec(),
    };
    // Best effort throughout: a queue too large for the state budget, or a host
    // with no state service at all, degrades to the full `list=1` poll.
    match serde_json::to_string(&cache) {
        Ok(encoded) if encoded.len() <= MAX_TORRENT_CACHE_BYTES => {
            let _ = var::set(TORRENT_CACHE_VAR_KEY, encoded);
        }
        _ => {
            let _ = var::remove(TORRENT_CACHE_VAR_KEY);
        }
    }
}

fn set_seed_config(
    config: &UTorrentConfig,
    hash: &str,
    request: &PluginDownloadClientAddRequest,
) -> Result<(), PluginError> {
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
    request_json(config, &params, "GET", None, None)?;
    Ok(())
}

fn set_label(config: &UTorrentConfig, hash: &str, label: &str) -> Result<(), PluginError> {
    request_json(
        config,
        &[
            ("action".to_string(), "setprops".to_string()),
            ("hash".to_string(), hash.to_string()),
            ("s".to_string(), "label".to_string()),
            ("v".to_string(), label.to_string()),
        ],
        "GET",
        None,
        None,
    )?;
    Ok(())
}

/// uTorrent only drops a label from its label list when the label is set and
/// then blanked in the same `setprops` (`UTorrentProxy.cs:158-170`).
fn remove_label(config: &UTorrentConfig, hash: &str, label: &str) -> Result<(), PluginError> {
    request_json(
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
        None,
    )?;
    Ok(())
}

fn request_json(
    config: &UTorrentConfig,
    params: &[(String, String)],
    method: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
) -> Result<UTorrentResponse, PluginError> {
    let response = request_with_auth(config, params, method, body, content_type, false)?;
    if response.body_text.trim().is_empty() {
        return Ok(UTorrentResponse::default());
    }
    serde_json::from_str(&response.body_text).map_err(|error| {
        detailed_error(
            PluginErrorCode::Temporary,
            "uTorrent returned a response Scryer could not read.",
            format!("{error}: {}", truncate(&response.body_text)),
        )
    })
}

const MULTIPART_BOUNDARY: &str = "scryer-utorrent-boundary";

fn post_multipart(
    config: &UTorrentConfig,
    params: &[(String, String)],
    field_name: &str,
    filename: &str,
    file_bytes: &[u8],
) -> Result<UTorrentResponse, PluginError> {
    request_json(
        config,
        params,
        "POST",
        Some(multipart_body(field_name, filename, file_bytes)),
        Some(&format!(
            "multipart/form-data; boundary={MULTIPART_BOUNDARY}"
        )),
    )
}

/// The request body only. The content type travels as a header
/// (`request_with_auth`), never as a line smuggled into the body.
fn multipart_body(field_name: &str, filename: &str, file_bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{}\"\r\n",
            filename.replace('"', "")
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/x-bittorrent\r\n\r\n");
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    body
}

fn user_agent() -> String {
    concat!("scryer-utorrent-plugin/", env!("CARGO_PKG_VERSION")).to_string()
}

fn request_with_auth(
    config: &UTorrentConfig,
    params: &[(String, String)],
    method: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
    reauth: bool,
) -> Result<RawResponse, PluginError> {
    let (token, cookie) = authenticate(config, reauth)?;
    let retry_body = body.clone();
    let mut query = vec![("token".to_string(), token)];
    query.extend_from_slice(params);
    let url = format!("{}?{}", config.gui_url, encode_query(&query));
    let mut request = HttpRequest::new(url)
        .with_method(method)
        .with_header("Cache-Control", "no-cache")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", user_agent());
    if !cookie.is_empty() {
        request = request.with_header("Cookie", cookie);
    }
    if let Some(authorization) = basic_auth(config) {
        request = request.with_header("Authorization", authorization);
    }
    if let Some(content_type) = content_type {
        request = request.with_header("Content-Type", content_type);
    }
    let response = http::request::<Vec<u8>>(&request, body)
        .map_err(|error| classify_transport_error(&error.to_string()))?;
    let status = response.status_code();
    // `UTorrentProxy.cs:215-226`: uTorrent answers a stale token with 400 as
    // readily as with 401, so both mean "log in again", once.
    if (status == 400 || status == 401) && !reauth {
        let _ = var::remove(TOKEN_VAR_KEY);
        let _ = var::remove(COOKIE_VAR_KEY);
        return request_with_auth(config, params, method, retry_body, content_type, true);
    }
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    if let Some(error) = classify_http_status(status, response.header("Location"), &body_text) {
        return Err(error);
    }
    Ok(RawResponse { body_text })
}

fn authenticate(config: &UTorrentConfig, force: bool) -> Result<(String, String), PluginError> {
    if !force
        && let Ok(Some(token)) = var::get::<String>(TOKEN_VAR_KEY)
        && !token.is_empty()
    {
        let cookie = var::get::<String>(COOKIE_VAR_KEY)
            .ok()
            .flatten()
            .unwrap_or_default();
        return Ok((token, cookie));
    }
    let mut request = HttpRequest::new(format!("{}token.html", config.gui_url))
        .with_method("GET")
        .with_header("Cache-Control", "no-cache")
        .with_header("User-Agent", user_agent());
    if let Some(authorization) = basic_auth(config) {
        request = request.with_header("Authorization", authorization);
    }
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| classify_transport_error(&error.to_string()))?;
    let status = response.status_code();
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    if status == 401 || status == 403 {
        // Sonarr's `DownloadClientAuthenticationException` (`UTorrentProxy.cs:275`).
        return Err(detailed_error(
            PluginErrorCode::AuthFailed,
            "Failed to authenticate with uTorrent. Check the Web UI username and password, and whether the host running Scryer is allowed by uTorrent's IP whitelist.",
            truncate(&body_text),
        ));
    }
    if let Some(error) = classify_http_status(status, response.header("Location"), &body_text) {
        return Err(error);
    }
    let token = parse_token(&body_text).ok_or_else(|| {
        detailed_error(
            PluginErrorCode::InvalidConfig,
            "uTorrent's token endpoint did not return a Web UI token; check the URL base and whether the Web UI is enabled.",
            truncate(&body_text),
        )
    })?;
    // uTorrent 3.x issues a GUID cookie alongside the token; builds behind some
    // reverse proxies do not, and the token alone still authorises the call.
    let cookie = response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("set-cookie"))
        .and_then(|(_, value)| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string();
    let _ = var::set(TOKEN_VAR_KEY, token.clone());
    let _ = var::set(COOKIE_VAR_KEY, cookie.clone());
    Ok((token, cookie))
}

fn parse_token(html: &str) -> Option<String> {
    if let Ok(doc) = Document::parse(html)
        && let Some(text) = doc
            .descendants()
            .find(|node| node.attribute("id") == Some("token"))
            .and_then(|node| node.text())
        && !text.trim().is_empty()
    {
        return Some(text.to_string());
    }
    html.split('>')
        .find_map(|part| part.split('<').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

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
        date_completed: values.get(24).and_then(value_i64).unwrap_or_default(),
    }
}

fn torrent_to_item(torrent: UTorrentTorrent) -> PluginDownloadItem {
    let state = map_state(&torrent);
    let output_path = output_path(&torrent);
    PluginDownloadItem {
        client_item_id: torrent.hash.clone(),
        download_id: None,
        info_hash: Some(torrent.hash.clone()),
        title: torrent.name.clone(),
        state,
        message: item_message(&torrent),
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
            seed_time_seconds: seed_time_seconds(&torrent, now_unix_seconds()),
            raw_status: Some(torrent.status.to_string()),
            status_reason: torrent.status_message.clone(),
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.size),
        remaining_size_bytes: Some(torrent.remaining),
        eta_seconds: (torrent.eta != -1).then_some(torrent.eta),
        progress_percent: Some(((torrent.progress as f64 / 10.0).round().clamp(0.0, 100.0)) as u8),
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files: Some(is_completed(&torrent)),
        can_remove: derive_can_remove(&torrent),
        removed: Some(false),
        raw_state: Some(torrent.status.to_string()),
        completed_at: completed_at(&torrent),
    }
}

/// `UTorrent.cs:151` uses the localised `DownloadClientUTorrentTorrentStateError`
/// ("uTorrent is reporting an error"). uTorrent also puts a reason in the
/// status-message column, which is strictly more useful when it is there.
fn item_message(torrent: &UTorrentTorrent) -> Option<String> {
    if status_has(torrent.status, STATUS_ERROR) {
        return Some(match torrent.status_message.as_deref() {
            Some(detail) => format!("uTorrent is reporting an error: {detail}"),
            None => "uTorrent is reporting an error".to_string(),
        });
    }
    torrent.status_message.clone()
}

/// `UTorrent.cs:137-146`: uTorrent's root download path already ends in the
/// torrent name for a single-file torrent it renamed, so appending it again
/// would invent a path.
fn output_path(torrent: &UTorrentTorrent) -> String {
    if last_path_segment(&torrent.root_download_path) == torrent.name {
        torrent.root_download_path.clone()
    } else {
        join_path(&torrent.root_download_path, &torrent.name)
    }
}

fn torrent_to_completed(torrent: UTorrentTorrent) -> PluginCompletedDownload {
    let output_path = output_path(&torrent);
    let completed_at = completed_at(&torrent);
    PluginCompletedDownload {
        client_item_id: torrent.hash.clone(),
        download_id: None,
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
        completed_at,
        parameters: Vec::new(),
        release_name: None,
    }
}

fn map_state(torrent: &UTorrentTorrent) -> DownloadItemState {
    if status_has(torrent.status, STATUS_ERROR) {
        DownloadItemState::Warning
    } else if is_completed(torrent) {
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

/// Honest `can_remove` for uTorrent.
///
/// uTorrent's `list=1` payload carries no seeding limits (those live behind per-torrent
/// `getprops`), so the only client-side fact available is whether uTorrent is still running
/// the torrent. A stopped, fully downloaded torrent is no longer seeding, so removing it
/// cannot interrupt seeding; anything uTorrent is still running is unfinished business, and
/// a user-paused or queued torrent is unknowable.
fn derive_can_remove(torrent: &UTorrentTorrent) -> Option<bool> {
    if !is_completed(torrent) {
        return Some(false);
    }
    if status_has(torrent.status, STATUS_STARTED) {
        // Still being seeded under whatever limits uTorrent holds privately.
        return Some(false);
    }
    if status_has(torrent.status, STATUS_PAUSED) || status_has(torrent.status, STATUS_QUEUED) {
        // Paused/queued by the user or the queue manager, not by a seeding limit.
        return None;
    }
    // Loaded but not started: uTorrent stopped the torrent, so it is no longer seeding.
    Some(true)
}

/// Seconds spent seeding, derived from uTorrent's "date completed" column.
fn seed_time_seconds(torrent: &UTorrentTorrent, now: i64) -> Option<i64> {
    (is_completed(torrent) && torrent.date_completed > 0)
        .then(|| now.saturating_sub(torrent.date_completed).max(0))
}

/// uTorrent reports the completion instant itself, which Sonarr's
/// `DownloadClientItem` has no field for.
fn completed_at(torrent: &UTorrentTorrent) -> Option<String> {
    is_completed(torrent)
        .then(|| unix_to_rfc3339(torrent.date_completed))
        .flatten()
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

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
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

fn basic_auth(config: &UTorrentConfig) -> Option<String> {
    (!config.username.is_empty() || !config.password.is_empty()).then(|| {
        format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", config.username, config.password))
        )
    })
}

fn value_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        // uTorrent leaves optional columns as JSON `null`; `to_string()` would
        // turn those into the literal text "null".
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn value_i64(value: &serde_json::Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

/// Join with the separator style the root already uses.
///
/// Sonarr's `OsPath` keeps a path's own flavour (fixture
/// `should_combine_drive_letter`: `D:` + title is `D:\title`). A mixed
/// `D:/title` would not prefix-match any remote path mapping.
fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        return name.to_string();
    }
    let separator = path_separator(dir);
    format!("{}{separator}{name}", dir.trim_end_matches(['/', '\\']))
}

fn path_separator(path: &str) -> char {
    if path.contains('\\') {
        '\\'
    } else if path.contains('/') {
        '/'
    } else if is_windows_drive(path) {
        '\\'
    } else {
        '/'
    }
}

fn is_windows_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn last_path_segment(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn path_looks_like_file(path: &str) -> bool {
    last_path_segment(path).contains('.')
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

/// `UTorrent.cs:245` tests the configured host, not the composed URL.
fn is_localhost_host(host: &str) -> bool {
    matches!(
        host.trim().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{PluginDownloadIsolation, PluginTorrentOptions};

    const NOW: i64 = 1_700_000_000;
    /// The fixture's `_title` stand-in.
    const TITLE: &str = "Droned.S01E01.Pilot.1080p.WEB-DL-DRONE";

    fn config() -> UTorrentConfig {
        UTorrentConfig {
            host: "127.0.0.1".to_string(),
            port: "2222".to_string(),
            gui_url: "http://127.0.0.1:2222/gui/".to_string(),
            username: "admin".to_string(),
            password: "pass".to_string(),
            category: "tv".to_string(),
            post_import_category: String::new(),
            recent_priority_first: false,
            older_priority_first: false,
            initial_state: "start".to_string(),
        }
    }

    /// `UTorrentFixture.cs:37-48`.
    fn queued_torrent(status: i64) -> UTorrentTorrent {
        UTorrentTorrent {
            hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            status,
            name: TITLE.to_string(),
            size: 1_000,
            remaining: 1_000,
            progress: 0,
            label: "tv".to_string(),
            root_download_path: "somepath".to_string(),
            ..UTorrentTorrent::default()
        }
    }

    /// `UTorrentFixture.cs:50-61`.
    fn downloading_torrent(status: i64) -> UTorrentTorrent {
        UTorrentTorrent {
            remaining: 100,
            progress: 900,
            ..queued_torrent(status)
        }
    }

    /// `UTorrentFixture.cs:76-87`.
    fn completed_torrent(status: i64) -> UTorrentTorrent {
        UTorrentTorrent {
            remaining: 0,
            progress: 1_000,
            downloaded: 1_000,
            uploaded: 1_500,
            ratio: 1_500,
            date_completed: NOW - 600,
            ..queued_torrent(status)
        }
    }

    // -----------------------------------------------------------------------
    // Status table — every row of the Sonarr fixture
    // -----------------------------------------------------------------------

    #[test]
    fn queued_rows_map_the_way_sonarr_maps_them() {
        // `UTorrentFixture.cs:268-272`.
        for (status, expected) in [
            (STATUS_LOADED, DownloadItemState::Queued),
            (STATUS_LOADED | 2, DownloadItemState::Queued),
            (STATUS_LOADED | STATUS_QUEUED, DownloadItemState::Queued),
            (
                STATUS_LOADED | STATUS_STARTED,
                DownloadItemState::Downloading,
            ),
            (
                STATUS_LOADED | STATUS_QUEUED | STATUS_STARTED,
                DownloadItemState::Downloading,
            ),
        ] {
            assert_eq!(map_state(&queued_torrent(status)), expected, "{status}");
        }
    }

    #[test]
    fn downloading_rows_map_the_way_sonarr_maps_them() {
        // `UTorrentFixture.cs:284-287`.
        for (status, expected) in [
            (STATUS_LOADED | 2, DownloadItemState::Queued),
            (
                STATUS_LOADED | STATUS_CHECKED | STATUS_QUEUED,
                DownloadItemState::Queued,
            ),
            (
                STATUS_LOADED | STATUS_STARTED,
                DownloadItemState::Downloading,
            ),
            (
                STATUS_LOADED | STATUS_QUEUED | STATUS_STARTED,
                DownloadItemState::Downloading,
            ),
        ] {
            assert_eq!(
                map_state(&downloading_torrent(status)),
                expected,
                "{status}"
            );
        }
    }

    #[test]
    fn completed_rows_map_the_way_sonarr_maps_them() {
        // `UTorrentFixture.cs:299-303`.
        for (status, expected) in [
            (STATUS_LOADED | 2, DownloadItemState::Queued),
            (STATUS_LOADED | STATUS_CHECKED, DownloadItemState::Completed),
            (
                STATUS_LOADED | STATUS_CHECKED | STATUS_QUEUED,
                DownloadItemState::Completed,
            ),
            (
                STATUS_LOADED | STATUS_CHECKED | STATUS_STARTED,
                DownloadItemState::Completed,
            ),
            (
                STATUS_LOADED | STATUS_CHECKED | STATUS_QUEUED | STATUS_PAUSED,
                DownloadItemState::Completed,
            ),
        ] {
            assert_eq!(map_state(&completed_torrent(status)), expected, "{status}");
        }
    }

    #[test]
    fn an_error_row_is_a_warning_carrying_the_status_message() {
        // `UTorrentFixture.cs:63-74` plus `UTorrent.cs:148-152`.
        let mut torrent = downloading_torrent(STATUS_ERROR);
        assert_eq!(map_state(&torrent), DownloadItemState::Warning);
        assert_eq!(
            item_message(&torrent).as_deref(),
            Some("uTorrent is reporting an error")
        );
        torrent.status_message = Some("Error: unable to load .torrent".to_string());
        assert_eq!(
            item_message(&torrent).as_deref(),
            Some("uTorrent is reporting an error: Error: unable to load .torrent")
        );
    }

    #[test]
    fn an_unrecognised_status_bit_keeps_polling_rather_than_alarming() {
        // Common rule 2: an unknown state never becomes Warning/Error/Failed.
        let torrent = queued_torrent(1 << 20);
        assert_eq!(map_state(&torrent), DownloadItemState::Queued);
    }

    // -----------------------------------------------------------------------
    // can_remove / can_move_files
    // -----------------------------------------------------------------------

    #[test]
    fn can_remove_is_false_while_downloading() {
        let torrent = downloading_torrent(STATUS_LOADED | STATUS_CHECKED | STATUS_STARTED);
        assert_eq!(derive_can_remove(&torrent), Some(false));
    }

    #[test]
    fn can_remove_is_false_while_utorrent_is_still_seeding() {
        // `UTorrentFixture.cs:302` — Sonarr says false here too.
        let torrent = completed_torrent(STATUS_LOADED | STATUS_CHECKED | STATUS_STARTED);
        assert_eq!(derive_can_remove(&torrent), Some(false));
    }

    #[test]
    fn can_remove_is_true_once_utorrent_stopped_the_finished_torrent() {
        // `UTorrentFixture.cs:300` — Sonarr says true here too.
        let torrent = completed_torrent(STATUS_LOADED | STATUS_CHECKED);
        assert_eq!(derive_can_remove(&torrent), Some(true));
    }

    #[test]
    fn can_remove_is_unknown_for_paused_or_queued_torrents() {
        let paused = completed_torrent(STATUS_LOADED | STATUS_CHECKED | STATUS_PAUSED);
        let queued = completed_torrent(STATUS_LOADED | STATUS_CHECKED | STATUS_QUEUED);
        assert_eq!(derive_can_remove(&paused), None);
        assert_eq!(derive_can_remove(&queued), None);
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let torrent = completed_torrent(STATUS_LOADED | STATUS_CHECKED | STATUS_STARTED);
        let item = torrent_to_item(torrent);
        assert_eq!(item.can_move_files, Some(true));
        assert_eq!(item.can_remove, Some(false));
    }

    #[test]
    fn is_private_is_never_claimed_because_utorrent_does_not_report_it() {
        let torrent = completed_torrent(STATUS_LOADED | STATUS_CHECKED);
        assert_eq!(torrent_to_item(torrent).torrent.unwrap().is_private, None);
    }

    #[test]
    fn observed_ratio_is_reported_in_whole_units() {
        let torrent = completed_torrent(STATUS_LOADED | STATUS_CHECKED);
        assert_eq!(
            torrent_to_item(torrent).torrent.unwrap().seed_ratio,
            Some(1.5)
        );
    }

    #[test]
    fn seed_time_is_derived_from_the_completion_timestamp() {
        let torrent = completed_torrent(STATUS_LOADED | STATUS_CHECKED | STATUS_STARTED);
        assert_eq!(seed_time_seconds(&torrent, NOW), Some(600));
        let never_completed = UTorrentTorrent {
            date_completed: 0,
            ..torrent
        };
        assert_eq!(seed_time_seconds(&never_completed, NOW), None);
    }

    // -----------------------------------------------------------------------
    // Paths
    // -----------------------------------------------------------------------

    #[test]
    fn a_windows_drive_letter_root_keeps_its_own_separator() {
        // `UTorrentFixture.cs:339-351` (`should_combine_drive_letter`).
        let torrent = UTorrentTorrent {
            root_download_path: "D:".to_string(),
            ..completed_torrent(STATUS_LOADED | STATUS_CHECKED)
        };
        assert_eq!(output_path(&torrent), format!("D:\\{TITLE}"));

        let torrent = UTorrentTorrent {
            root_download_path: "D:\\".to_string(),
            ..completed_torrent(STATUS_LOADED | STATUS_CHECKED)
        };
        assert_eq!(output_path(&torrent), format!("D:\\{TITLE}"));

        let torrent = UTorrentTorrent {
            root_download_path: "C:\\Downloads\\Finished".to_string(),
            ..completed_torrent(STATUS_LOADED | STATUS_CHECKED)
        };
        assert_eq!(
            output_path(&torrent),
            format!("C:\\Downloads\\Finished\\{TITLE}")
        );
    }

    #[test]
    fn a_posix_root_keeps_forward_slashes() {
        let torrent = UTorrentTorrent {
            root_download_path: "/downloads/complete".to_string(),
            ..completed_torrent(STATUS_LOADED | STATUS_CHECKED)
        };
        assert_eq!(
            output_path(&torrent),
            format!("/downloads/complete/{TITLE}")
        );
    }

    #[test]
    fn a_root_that_already_ends_in_the_torrent_name_is_used_as_is() {
        // `UTorrent.cs:139-142`.
        let torrent = UTorrentTorrent {
            root_download_path: format!("/downloads/{TITLE}"),
            ..completed_torrent(STATUS_LOADED | STATUS_CHECKED)
        };
        assert_eq!(output_path(&torrent), format!("/downloads/{TITLE}"));
    }

    #[test]
    fn the_output_root_chain_matches_sonarrs() {
        // `UTorrentFixture.cs:317-337` (`should_return_status_with_outputdirs`).
        let mut settings = std::collections::HashMap::new();
        settings.insert("dir_active_download_flag".to_string(), "true".to_string());
        settings.insert(
            "dir_active_download".to_string(),
            "C:\\Downloads\\Downloading\\utorrent".to_string(),
        );
        settings.insert(
            "dir_completed_download".to_string(),
            "C:\\Downloads\\Finished\\utorrent".to_string(),
        );
        settings.insert(
            "dir_completed_download_flag".to_string(),
            "true".to_string(),
        );
        settings.insert("dir_add_label".to_string(), "true".to_string());
        assert_eq!(
            output_root(&settings, "tv"),
            "C:\\Downloads\\Finished\\utorrent\\tv"
        );

        settings.insert("dir_add_label".to_string(), "false".to_string());
        assert_eq!(
            output_root(&settings, "tv"),
            "C:\\Downloads\\Finished\\utorrent"
        );

        settings.insert(
            "dir_completed_download_flag".to_string(),
            "false".to_string(),
        );
        assert_eq!(
            output_root(&settings, "tv"),
            "C:\\Downloads\\Downloading\\utorrent"
        );

        settings.insert("dir_active_download_flag".to_string(), "false".to_string());
        assert!(output_root(&settings, "tv").is_empty());
    }

    #[test]
    fn localhost_is_decided_from_the_configured_host() {
        assert!(is_localhost_host("127.0.0.1"));
        assert!(is_localhost_host("localhost"));
        assert!(!is_localhost_host("seedbox.example"));
    }

    // -----------------------------------------------------------------------
    // Info-hash derivation
    // -----------------------------------------------------------------------

    fn add_request(source_json: &str) -> PluginDownloadClientAddRequest {
        serde_json::from_str(&format!(
            r#"{{
                "source": {source_json},
                "release": {{}},
                "title": {{"title_name": "Droned", "media_facet": "tv"}},
                "routing": {{}}
            }}"#
        ))
        .expect("test add request must parse")
    }

    #[test]
    fn a_base32_magnet_yields_the_hash_sonarr_computes() {
        // `UTorrentFixture.cs:255` — the exact pair Sonarr pins.
        let request = add_request(
            r#"{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn:btih:ZPBPA2P6ROZPKRHK44D5OW6NHXU5Z6KR&tr=udp"}"#,
        );
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some("cbc2f069fe8bb2f544eae707d75bcd3de9dcf951")
        );
    }

    #[test]
    fn a_hex_magnet_yields_the_same_hash_lowercased() {
        let request = add_request(
            r#"{"kind":"magnet_uri","download_url":"magnet:?dn=x&xt=urn:btih:CBC2F069FE8BB2F544EAE707D75BCD3DE9DCF951"}"#,
        );
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some("cbc2f069fe8bb2f544eae707d75bcd3de9dcf951")
        );
    }

    #[test]
    fn a_percent_encoded_magnet_urn_is_decoded_first() {
        let request = add_request(
            r#"{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn%3Abtih%3AZPBPA2P6ROZPKRHK44D5OW6NHXU5Z6KR"}"#,
        );
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some("cbc2f069fe8bb2f544eae707d75bcd3de9dcf951")
        );
    }

    #[test]
    fn the_release_hash_wins_over_the_source() {
        let mut request = add_request(
            r#"{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn:btih:ZPBPA2P6ROZPKRHK44D5OW6NHXU5Z6KR"}"#,
        );
        request.release.info_hash_v1 = Some("00112233445566778899AABBCCDDEEFF00112233".to_string());
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some("00112233445566778899aabbccddeeff00112233")
        );
    }

    #[test]
    fn a_torrent_body_is_hashed_over_its_info_dictionary() {
        // `d8:announce3:foo4:infod6:lengthi3e4:name3:bazee` — the `info` value is
        // `d6:lengthi3e4:name3:baze`.
        let torrent = b"d8:announce3:foo4:infod6:lengthi3e4:name3:bazee";
        let info = b"d6:lengthi3e4:name3:baze";
        let mut hasher = Sha1::new();
        hasher.update(info);
        let expected = to_lower_hex(&hasher.finalize());

        let request = add_request(&format!(
            r#"{{"kind":"torrent_bytes","torrent_bytes_base64":"{}"}}"#,
            STANDARD.encode(torrent)
        ));
        assert_eq!(
            derive_info_hash(&request).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn a_plain_torrent_url_with_no_hash_anywhere_cannot_be_added() {
        let request = add_request(
            r#"{"kind":"torrent_url","torrent_url":"https://indexer.example/download/1"}"#,
        );
        assert_eq!(derive_info_hash(&request), None);
    }

    #[test]
    fn a_truncated_torrent_body_does_not_produce_a_hash() {
        let request = add_request(&format!(
            r#"{{"kind":"torrent_bytes","torrent_bytes_base64":"{}"}}"#,
            STANDARD.encode(b"d8:announce3:foo4:infod6:length")
        ));
        assert_eq!(derive_info_hash(&request), None);
    }

    // -----------------------------------------------------------------------
    // Add-time routing
    // -----------------------------------------------------------------------

    #[test]
    fn a_magnet_request_hands_utorrent_the_magnet() {
        let request = add_request(
            r#"{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn:btih:ZPBPA2P6ROZPKRHK44D5OW6NHXU5Z6KR","torrent_bytes_base64":"ZA=="}"#,
        );
        assert!(
            matches!(add_payload(&request), Some(AddPayload::Url(url)) if url.starts_with("magnet:"))
        );
    }

    #[test]
    fn a_torrent_bytes_request_uploads_the_body() {
        let request = add_request(
            r#"{"kind":"torrent_bytes","torrent_bytes_base64":"ZA==","torrent_url":"https://indexer.example/1"}"#,
        );
        assert!(matches!(add_payload(&request), Some(AddPayload::Bytes(body)) if body == "ZA=="));
    }

    #[test]
    fn a_torrent_url_request_falls_back_to_the_body_when_there_is_no_url() {
        let request = add_request(r#"{"kind":"torrent_url","torrent_bytes_base64":"ZA=="}"#);
        assert!(matches!(add_payload(&request), Some(AddPayload::Bytes(body)) if body == "ZA=="));
    }

    #[test]
    fn a_source_with_nothing_in_it_is_not_addable() {
        let request = add_request(r#"{"kind":"torrent_url"}"#);
        assert!(add_payload(&request).is_none());
    }

    fn torrent_options(options: PluginTorrentOptions) -> PluginDownloadClientAddRequest {
        let mut request = add_request(
            r#"{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn:btih:ZPBPA2P6ROZPKRHK44D5OW6NHXU5Z6KR"}"#,
        );
        request.torrent = Some(options);
        request
    }

    #[test]
    fn the_configured_initial_state_applies_when_the_core_routes_none() {
        // `UTorrent.cs:78`.
        let request = add_request(r#"{"kind":"magnet_uri","magnet_uri":"magnet:?x"}"#);
        assert_eq!(
            initial_state_action(&config(), &request).as_deref(),
            Some("start")
        );
        let paused = UTorrentConfig {
            initial_state: "pause".to_string(),
            ..config()
        };
        assert_eq!(
            initial_state_action(&paused, &request).as_deref(),
            Some("pause")
        );
    }

    #[test]
    fn a_routed_initial_state_beats_the_configured_default() {
        for (state, expected) in [
            (PluginTorrentInitialState::Paused, "pause"),
            (PluginTorrentInitialState::Stopped, "stop"),
            (PluginTorrentInitialState::Started, "start"),
        ] {
            let request = torrent_options(PluginTorrentOptions {
                initial_state: Some(state),
                ..PluginTorrentOptions::default()
            });
            let config = UTorrentConfig {
                initial_state: "pause".to_string(),
                ..config()
            };
            assert_eq!(
                initial_state_action(&config, &request).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn force_start_is_honoured_on_its_own_and_alongside_started() {
        let request = torrent_options(PluginTorrentOptions {
            force_start: Some(true),
            ..PluginTorrentOptions::default()
        });
        assert_eq!(
            initial_state_action(&config(), &request).as_deref(),
            Some("forcestart")
        );

        let request = torrent_options(PluginTorrentOptions {
            initial_state: Some(PluginTorrentInitialState::Started),
            force_start: Some(true),
            ..PluginTorrentOptions::default()
        });
        assert_eq!(
            initial_state_action(&config(), &request).as_deref(),
            Some("forcestart")
        );

        // An explicit pause still wins: force-starting something the core asked
        // to keep paused would defeat the request.
        let request = torrent_options(PluginTorrentOptions {
            initial_state: Some(PluginTorrentInitialState::Paused),
            force_start: Some(true),
            ..PluginTorrentOptions::default()
        });
        assert_eq!(
            initial_state_action(&config(), &request).as_deref(),
            Some("pause")
        );
    }

    #[test]
    fn recent_and_older_priority_move_a_grab_to_the_top() {
        // `UTorrent.cs:70-76`.
        let recent_first = UTorrentConfig {
            recent_priority_first: true,
            ..config()
        };
        let mut request = add_request(r#"{"kind":"magnet_uri","magnet_uri":"magnet:?x"}"#);
        request.release.is_recent = Some(true);
        assert_eq!(queue_action(&recent_first, &request), Some("queuetop"));
        request.release.is_recent = Some(false);
        assert_eq!(queue_action(&recent_first, &request), None);

        let older_first = UTorrentConfig {
            older_priority_first: true,
            ..config()
        };
        assert_eq!(queue_action(&older_first, &request), Some("queuetop"));
    }

    #[test]
    fn an_explicit_queue_placement_beats_the_configured_priority() {
        let recent_first = UTorrentConfig {
            recent_priority_first: true,
            ..config()
        };
        let mut request = torrent_options(PluginTorrentOptions {
            queue_placement: Some(PluginTorrentQueuePlacement::Last),
            ..PluginTorrentOptions::default()
        });
        request.release.is_recent = Some(true);
        assert_eq!(queue_action(&recent_first, &request), Some("queuebottom"));

        let request = torrent_options(PluginTorrentOptions {
            queue_placement: Some(PluginTorrentQueuePlacement::First),
            ..PluginTorrentOptions::default()
        });
        assert_eq!(queue_action(&config(), &request), Some("queuetop"));
    }

    // -----------------------------------------------------------------------
    // Post-import handoff
    // -----------------------------------------------------------------------

    fn mark_request(json: &str) -> PluginDownloadClientMarkImportedRequest {
        serde_json::from_str(json).expect("test mark-imported request must parse")
    }

    #[test]
    fn no_post_import_category_means_no_post_import_work() {
        // Sonarr's outer guard (`UTorrent.cs:47`).
        let request = mark_request(r#"{"client_item_id":"abc","category":"tv"}"#);
        assert_eq!(post_import_plan(&config(), &request), None);
    }

    #[test]
    fn the_configured_label_is_applied_and_the_routed_one_is_replaced() {
        // The core fills `post_import_isolation` with the download's own grab
        // category, so it is the label to drop, not the label to apply.
        let config = UTorrentConfig {
            post_import_category: "tv-imported".to_string(),
            ..config()
        };
        let request = mark_request(
            r#"{"client_item_id":"abc","category":"tv","post_import_isolation":[{"mode":"tag","value":"tv"}]}"#,
        );
        assert_eq!(
            post_import_plan(&config, &request),
            Some(PostImportPlan {
                imported_label: "tv-imported".to_string(),
                scope_label: Some("tv".to_string()),
            })
        );
    }

    #[test]
    fn the_scope_label_falls_back_to_the_tracked_then_configured_category() {
        let config = UTorrentConfig {
            post_import_category: "tv-imported".to_string(),
            ..config()
        };
        let tracked = mark_request(r#"{"client_item_id":"abc","category":"Scryer-TV"}"#);
        assert_eq!(
            post_import_plan(&config, &tracked).unwrap().scope_label,
            Some("Scryer-TV".to_string())
        );
        let bare = mark_request(r#"{"client_item_id":"abc"}"#);
        assert_eq!(
            post_import_plan(&config, &bare).unwrap().scope_label,
            Some("tv".to_string())
        );
    }

    #[test]
    fn an_imported_label_equal_to_the_grab_label_is_a_no_op() {
        // Sonarr's `TvImportedCategory != TvCategory` guard.
        let config = UTorrentConfig {
            post_import_category: "tv".to_string(),
            ..config()
        };
        let request = mark_request(
            r#"{"client_item_id":"abc","post_import_isolation":[{"mode":"tag","value":"TV"}]}"#,
        );
        assert_eq!(post_import_plan(&config, &request), None);
    }

    #[test]
    fn only_label_shaped_isolation_entries_are_read() {
        let config = UTorrentConfig {
            post_import_category: "tv-imported".to_string(),
            ..config()
        };
        let mut request = mark_request(r#"{"client_item_id":"abc"}"#);
        request.post_import_isolation = vec![PluginDownloadIsolation {
            mode: DownloadIsolationMode::Directory,
            value: "/data/imported".to_string(),
        }];
        // Falls through to the configured category rather than treating a
        // directory as a label.
        assert_eq!(
            post_import_plan(&config, &request).unwrap().scope_label,
            Some("tv".to_string())
        );
    }

    #[test]
    fn the_descriptor_advertises_the_non_destructive_handoff() {
        let descriptor: serde_json::Value =
            serde_json::from_str(&scryer_describe(String::new()).unwrap()).unwrap();
        assert_eq!(
            descriptor["provider"]["capabilities"]["mark_imported_non_destructive"],
            serde_json::json!(true)
        );
        assert_eq!(
            descriptor["provider"]["capabilities"]["torrent"]["supports_queue_placement"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn the_function_table_wires_the_non_destructive_handoff() {
        assert!(functions().mark_imported_non_destructive.is_some());
    }

    // -----------------------------------------------------------------------
    // Differential polling
    // -----------------------------------------------------------------------

    fn list_row(hash: &str, status: i64, name: &str) -> Vec<serde_json::Value> {
        let mut row: Vec<serde_json::Value> = (0..27).map(|_| serde_json::json!(0)).collect();
        row[0] = serde_json::json!(hash);
        row[1] = serde_json::json!(status);
        row[2] = serde_json::json!(name);
        row[11] = serde_json::json!("tv");
        row[26] = serde_json::json!("/downloads");
        row
    }

    fn response(json: serde_json::Value) -> UTorrentResponse {
        serde_json::from_value(json).expect("test response must parse")
    }

    #[test]
    fn a_full_answer_replaces_the_cache() {
        let cache = TorrentCache {
            key: "127.0.0.1:2222:tv".to_string(),
            cache_id: "abc".to_string(),
            stored_at: NOW,
            torrents: vec![queued_torrent(STATUS_LOADED)],
        };
        let response = response(serde_json::json!({
            "torrents": [list_row("AAAA", STATUS_LOADED, "fresh")],
            "torrentc": "abc2",
        }));
        let merged = merge_torrents(Some(&cache), &response);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "fresh");
    }

    #[test]
    fn a_differential_answer_is_merged_into_the_cache() {
        // `UTorrentFixture.cs:379-401` (`GetItems_should_query_with_cache_id_if_available`)
        // and `UTorrent.cs:192-200`.
        let kept = UTorrentTorrent {
            hash: "aaaa".to_string(),
            name: "kept".to_string(),
            ..queued_torrent(STATUS_LOADED)
        };
        let replaced = UTorrentTorrent {
            hash: "bbbb".to_string(),
            name: "stale".to_string(),
            ..queued_torrent(STATUS_LOADED)
        };
        let dropped = UTorrentTorrent {
            hash: "cccc".to_string(),
            name: "dropped".to_string(),
            ..queued_torrent(STATUS_LOADED)
        };
        let cache = TorrentCache {
            key: "127.0.0.1:2222:tv".to_string(),
            cache_id: "abc".to_string(),
            stored_at: NOW,
            torrents: vec![kept, replaced, dropped],
        };
        let response = response(serde_json::json!({
            "torrentp": [list_row("BBBB", STATUS_LOADED | STATUS_STARTED, "refreshed")],
            "torrentm": ["CCCC"],
            "torrentc": "abc2",
        }));

        let merged = merge_torrents(Some(&cache), &response);
        let names: Vec<&str> = merged.iter().map(|torrent| torrent.name.as_str()).collect();
        assert_eq!(names, vec!["kept", "refreshed"]);
        assert_eq!(merged[1].hash, "bbbb");
    }

    #[test]
    fn a_differential_answer_without_a_cache_is_taken_at_face_value() {
        let response = response(serde_json::json!({
            "torrentp": [list_row("BBBB", STATUS_LOADED, "only")],
            "torrentc": "abc",
        }));
        let merged = merge_torrents(None, &response);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "only");
    }

    #[test]
    fn a_cache_entry_expires_and_is_scoped_to_its_client() {
        // Sonarr's 15-minute window (`UTorrent.cs:212`) and its
        // `host:port:category` key (`:187`).
        assert_eq!(config().cache_key(), "127.0.0.1:2222:tv");
        let cache = TorrentCache {
            key: config().cache_key(),
            cache_id: "abc".to_string(),
            stored_at: NOW,
            torrents: Vec::new(),
        };
        assert!(cache_is_usable(&cache, &config().cache_key(), NOW));
        assert!(cache_is_usable(
            &cache,
            &config().cache_key(),
            NOW + TORRENT_CACHE_TTL_SECONDS - 1
        ));
        assert!(!cache_is_usable(
            &cache,
            &config().cache_key(),
            NOW + TORRENT_CACHE_TTL_SECONDS
        ));
        assert!(!cache_is_usable(&cache, "127.0.0.1:2222:movies", NOW));
        assert!(!cache_is_usable(
            &TorrentCache {
                cache_id: String::new(),
                ..cache
            },
            &config().cache_key(),
            NOW
        ));
    }

    // -----------------------------------------------------------------------
    // Row mapping
    // -----------------------------------------------------------------------

    #[test]
    fn list_row_maps_the_date_completed_column() {
        let mut row = list_row("ABCDEF0123456789ABCDEF0123456789ABCDEF01", 0, "Movie");
        row[1] = serde_json::json!(STATUS_LOADED | STATUS_CHECKED);
        row[24] = serde_json::json!(1_699_999_000_i64);
        let torrent = map_torrent(row);
        assert_eq!(torrent.date_completed, 1_699_999_000);
        assert_eq!(torrent.hash, "abcdef0123456789abcdef0123456789abcdef01");
    }

    #[test]
    fn a_null_column_is_empty_rather_than_the_text_null() {
        let mut row = list_row("AAAA", STATUS_LOADED, "Movie");
        row[21] = serde_json::Value::Null;
        row[26] = serde_json::Value::Null;
        let torrent = map_torrent(row);
        assert_eq!(torrent.status_message, None);
        assert_eq!(torrent.root_download_path, "");
    }

    #[test]
    fn a_completed_torrent_reports_its_completion_time() {
        let torrent = completed_torrent(STATUS_LOADED | STATUS_CHECKED);
        let expected = unix_to_rfc3339(NOW - 600);
        assert_eq!(completed_at(&torrent), expected);
        assert_eq!(torrent_to_completed(torrent.clone()).completed_at, expected);
        assert_eq!(torrent_to_item(torrent).completed_at, expected);
    }

    #[test]
    fn an_unfinished_torrent_reports_no_completion_time() {
        let torrent = downloading_torrent(STATUS_LOADED | STATUS_STARTED);
        assert_eq!(completed_at(&torrent), None);
    }

    #[test]
    fn unix_seconds_become_rfc3339() {
        assert_eq!(unix_to_rfc3339(0), None);
        assert_eq!(unix_to_rfc3339(-5), None);
        assert_eq!(
            unix_to_rfc3339(1_700_000_000).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
    }

    // -----------------------------------------------------------------------
    // Error classification
    // -----------------------------------------------------------------------

    #[test]
    fn http_statuses_carry_sonarrs_distinctions() {
        for (status, expected) in [
            (401_u16, PluginErrorCode::AuthFailed),
            (403, PluginErrorCode::AuthFailed),
            (404, PluginErrorCode::InvalidConfig),
            (429, PluginErrorCode::Temporary),
            (500, PluginErrorCode::Temporary),
            (418, PluginErrorCode::Permanent),
        ] {
            let error = classify_http_status(status, None, "body").expect("status is a failure");
            assert_eq!(error.code, expected, "status {status}");
        }
        assert!(classify_http_status(200, None, "").is_none());
    }

    #[test]
    fn a_redirect_to_a_login_page_is_a_configuration_problem() {
        let error = classify_http_status(302, Some("/login"), "").expect("redirect is a failure");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("/login"));
    }

    #[test]
    fn transport_failures_are_named_the_way_sonarr_names_them() {
        assert_eq!(
            classify_transport_error("certificate verify failed").code,
            PluginErrorCode::UpstreamUnavailable
        );
        assert_eq!(
            classify_transport_error("request timeout").code,
            PluginErrorCode::Temporary
        );
        assert_eq!(
            classify_transport_error("connection refused").code,
            PluginErrorCode::UpstreamUnavailable
        );
    }

    #[test]
    fn the_history_listing_is_empty_because_utorrent_keeps_none() {
        let raw = scryer_download_list_history(String::new()).unwrap();
        let result: PluginResult<Vec<PluginDownloadItem>> = serde_json::from_str(&raw).unwrap();
        assert!(matches!(result, PluginResult::Ok(items) if items.is_empty()));
    }

    #[test]
    fn a_multipart_body_starts_at_its_boundary() {
        // The request content type travels as a header; the body must not open
        // with a `Content-Type:` line for the transport to strip back out.
        let body = multipart_body(
            "torrent_file",
            "Some \"Release\".torrent",
            b"d4:infod1:xi1eee",
        );
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("--scryer-utorrent-boundary\r\n"), "{text}");
        assert!(text.contains("filename=\"Some Release.torrent\""), "{text}");
        assert!(
            text.ends_with("\r\n--scryer-utorrent-boundary--\r\n"),
            "{text}"
        );
        assert!(!text.starts_with("Content-Type:"), "{text}");
    }

    #[test]
    fn the_token_is_read_out_of_utorrents_html() {
        assert_eq!(
            parse_token("<html><div id='token' style='display:none;'>ABCDEF</div></html>")
                .as_deref(),
            Some("ABCDEF")
        );
        assert_eq!(parse_token("").as_deref(), None);
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
