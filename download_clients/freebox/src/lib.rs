//! Freebox Download — the BitTorrent half of the Freebox OS download manager.
//!
//! Reconciled against Sonarr's `TorrentFreeboxDownload` /
//! `FreeboxDownloadProxy` and against the Freebox OS developer documentation
//! (<https://dev.freebox.fr/sdk/os/>, sections *Login*, *Download*, *Download
//! Configuration*). Where the two disagree the documentation wins and the
//! divergence is called out at the site.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
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
use serde::de::DeserializeOwned;
use sha1::Sha1;

const SESSION_VAR_KEY: &str = "freebox.session_token";

/// Sonarr sends its own product/version; the port shipped a hard-coded `0.1`
/// that never moved with the crate.
const USER_AGENT: &str = concat!("scryer-freebox-plugin/", env!("CARGO_PKG_VERSION"));

macro_rules! warn_log {
    ($($argument:tt)*) => {
        scryer_plugin_pdk::log::log(
            scryer_plugin_pdk::log::LogLevel::Warn,
            &format!($($argument)*),
        )
    };
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// `Err(Error::msg(..))` reaches the host as `PluginErrorCode::Temporary`
/// (`pdk/scryer-plugin-pdk/src/download_client_bridge.rs:227-235`), so every
/// failure this plugin can name carries its own code instead (`00-common.md`
/// rule 4, mirroring `FreeboxDownloadProxy.cs:222-275` and
/// `TorrentFreeboxDownload.cs:168-188`).
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

/// A failure from the Freebox API, keeping the `error_code` the box sent so a
/// caller can act on it (an `exists` on add is a duplicate, not a failure)
/// while every other caller just converts it into the typed `PluginError`.
#[derive(Debug, Clone)]
struct ApiError {
    code: Option<String>,
    /// Boxed: a `PluginError` is over a hundred bytes, and every request
    /// helper returns `Result<_, ApiError>`, so an inline payload would make
    /// each `Err` variant larger than clippy's `result_large_err` ceiling.
    error: Box<PluginError>,
}

impl ApiError {
    fn new(error: PluginError) -> Self {
        Self {
            code: None,
            error: Box::new(error),
        }
    }

    fn with_code(code: &str, error: PluginError) -> Self {
        Self {
            code: Some(code.to_string()),
            error: Box::new(error),
        }
    }

    fn is(&self, code: &str) -> bool {
        self.code.as_deref() == Some(code)
    }
}

impl From<ApiError> for PluginError {
    fn from(value: ApiError) -> Self {
        *value.error
    }
}

fn respond<T: serde::Serialize>(result: Result<T, PluginError>) -> FnResult<String> {
    let result = match result {
        Ok(value) => PluginResult::Ok(value),
        Err(error) => PluginResult::Err(error),
    };
    Ok(serde_json::to_string(&result)?)
}

fn parse_request<T: DeserializeOwned>(input: &str) -> Result<T, PluginError> {
    serde_json::from_str(input).map_err(|error| {
        detailed_error(
            PluginErrorCode::Permanent,
            "Scryer sent a request this plugin could not read.",
            error.to_string(),
        )
    })
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// `APIResponse` (dev.freebox.fr/sdk/os/, *API conventions*).
#[derive(Default, Deserialize)]
struct FreeboxResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
}

/// `FreeboxLogin` plus the `permissions` object the session response carries
/// (dev.freebox.fr/sdk/os/login/, *Opening a session*), which Sonarr ignores.
#[derive(Default, Deserialize)]
struct FreeboxLogin {
    #[serde(default)]
    challenge: String,
    #[serde(default)]
    session_token: String,
    #[serde(default)]
    permissions: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Default, Deserialize)]
struct FreeboxDownloadConfiguration {
    #[serde(default, rename = "download_dir", deserialize_with = "nullable_string")]
    download_directory: String,
}

/// `/api_version` on the HTTP root — outside the API base and outside the
/// `APIResponse` envelope (dev.freebox.fr/sdk/os/, *Api Discovery using HTTP*).
#[derive(Default, Deserialize)]
struct FreeboxApiVersion {
    #[serde(default, deserialize_with = "nullable_string")]
    api_version: String,
    #[serde(default, deserialize_with = "nullable_string")]
    api_base_url: String,
}

/// The `Download` object (dev.freebox.fr/sdk/os/download/, *Download object*).
///
/// `id` is documented `int` and every example carries it unquoted; Sonarr's
/// `string Id` works only because Newtonsoft coerces. serde does not, so the
/// port failed to decode *every* task list and *every* add response against a
/// real box. `flexible_id` accepts both shapes.
#[derive(Default, Deserialize, Clone)]
struct FreeboxDownloadTask {
    #[serde(default, deserialize_with = "flexible_id")]
    id: String,
    #[serde(default, deserialize_with = "nullable_string")]
    name: String,
    #[serde(default, rename = "download_dir", deserialize_with = "nullable_string")]
    download_directory: String,
    #[serde(default, rename = "info_hash", deserialize_with = "nullable_string")]
    info_hash: String,
    #[serde(default, deserialize_with = "nullable_string")]
    status: String,
    #[serde(default)]
    eta: i64,
    #[serde(default, deserialize_with = "nullable_string")]
    error: String,
    #[serde(default, rename = "type", deserialize_with = "nullable_string")]
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

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FreeboxConfig {
    scheme: String,
    host: String,
    port: String,
    api_url: String,
    api_root: String,
    app_id: String,
    app_token: String,
    destination_directory: String,
    category: String,
    recent_priority: PluginTorrentQueuePlacement,
    older_priority: PluginTorrentQueuePlacement,
    add_paused: bool,
}

impl FreeboxConfig {
    fn from_host() -> Self {
        let host = config_value("host").unwrap_or_else(|| "mafreebox.freebox.fr".to_string());
        let port = config_value("port").unwrap_or_else(|| "443".to_string());
        let api_url = config_value("api_url").unwrap_or_else(|| "/api/v1/".to_string());
        let scheme = if config_bool("use_ssl", true) {
            "https"
        } else {
            "http"
        }
        .to_string();
        let api_root = format!("{scheme}://{host}:{port}/{}", api_url.trim_matches('/'));
        Self {
            scheme,
            host,
            port,
            api_url,
            api_root,
            app_id: config_value("app_id").unwrap_or_default(),
            app_token: config_value("app_token").unwrap_or_default(),
            destination_directory: config_value("destination_directory").unwrap_or_default(),
            category: config_value("category").unwrap_or_default(),
            recent_priority: placement(config_value("recent_priority").as_deref()),
            older_priority: placement(config_value("older_priority").as_deref()),
            add_paused: config_bool("add_paused", false),
        }
    }

    /// The HTTP root, which is where `/api_version` lives — not under the API
    /// base (dev.freebox.fr/sdk/os/, *Api Discovery using HTTP*).
    fn root_url(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }
}

fn placement(value: Option<&str>) -> PluginTorrentQueuePlacement {
    match value.map(str::to_ascii_lowercase).as_deref() {
        Some("first") | Some("top") | Some("high") => PluginTorrentQueuePlacement::First,
        Some("last") | Some("bottom") | Some("low") => PluginTorrentQueuePlacement::Last,
        _ => PluginTorrentQueuePlacement::Default,
    }
}

/// `FreeboxDownloadSettingsValidator` (`FreeboxDownloadSettings.cs:10-33`).
/// Scryer has no separate settings validator, so the rules run at
/// `test_connection` time. The ones that would make the box reject the add
/// also run before an add commits anything (`add_settings_problem`); the
/// category/destination exclusivity is reported by the test only, because
/// the destination wins consistently in both the add path and the scope
/// filter, so the combination cannot misroute a grab.
fn settings_problem(config: &FreeboxConfig) -> Option<String> {
    add_settings_problem(config).or_else(|| scope_exclusivity_problem(config))
}

fn scope_exclusivity_problem(config: &FreeboxConfig) -> Option<String> {
    (!config.category.is_empty() && !config.destination_directory.is_empty())
        .then(|| "Cannot use 'Category' and 'Destination Directory' at the same time.".to_string())
}

fn add_settings_problem(config: &FreeboxConfig) -> Option<String> {
    if config.host.is_empty() {
        return Some("'Host' must not be empty.".to_string());
    }
    match config.port.parse::<u32>() {
        Ok(port) if (1..=65535).contains(&port) => {}
        _ => return Some("'Port' must be a number between 1 and 65535.".to_string()),
    }
    if config.api_url.trim().is_empty() {
        return Some("'API URL' must not be empty.".to_string());
    }
    if !is_valid_url_base(&config.api_url) {
        return Some("'API URL' must be a valid URL path (ie: '/api/v1/').".to_string());
    }
    if config.app_id.is_empty() {
        return Some("'App ID' must not be empty.".to_string());
    }
    if config.app_token.is_empty() {
        return Some("'App Token' must not be empty.".to_string());
    }
    if !config.category.is_empty() && !is_valid_category(&config.category) {
        return Some("'Category' allows the characters a-z and - only.".to_string());
    }
    if !config.destination_directory.is_empty() && !config.destination_directory.starts_with('/') {
        return Some("'Destination' must be an absolute path on the Freebox.".to_string());
    }
    None
}

/// `ValidUrlBase` (`RuleBuilderExtensions.cs:55-58`): a path, never an absolute
/// URL.
fn is_valid_url_base(value: &str) -> bool {
    let lowered = value.trim().trim_start_matches('/').to_ascii_lowercase();
    !(lowered.starts_with("http://") || lowered.starts_with("https://"))
}

/// `^\.?[-a-z]*$` with `RegexOptions.IgnoreCase`
/// (`FreeboxDownloadSettings.cs:23-24`).
fn is_valid_category(value: &str) -> bool {
    let rest = value.strip_prefix('.').unwrap_or(value);
    rest.chars().all(|ch| ch == '-' || ch.is_ascii_alphabetic())
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

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
            // Freebox OS has no tag, label or view: the "category" setting is a
            // directory segment appended to the download root
            // (`TorrentFreeboxDownload.cs:190-215`), so directory isolation is
            // the only mode this client can actually honour.
            isolation_modes: vec![DownloadIsolationMode::Directory],
            capabilities: DownloadClientCapabilities {
                // `PUT /downloads/{id}` moves a task between `stopped` and
                // `downloading` (dev.freebox.fr/sdk/os/download/, *status*).
                // Sonarr has no pause/resume for any client; Scryer's contract
                // does, and Freebox answers it.
                pause: true,
                resume: true,
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
                    isolation_modes: vec![DownloadIsolationMode::Directory],
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
                // SDK 3.10 addition. `false` is the SDK's own default and therefore exactly
                // what this client's pre-3.10 descriptor already meant to a 3.10 host;
                // advertising category-scoped feedback would be a behaviour change, not a
                // transport one, so it stays off across the component migration.
                category_scoped_feedback: false,
                // Freebox OS has no tag, label, category or view to write back
                // to after an import, so there is no non-destructive handoff to
                // perform (`00-common.md` rule 3, same shape as aria2). The
                // core reads this before scheduling a mark, and the function
                // table leaves the non-destructive slot empty so the bridge
                // answers `Ok(())` itself.
                mark_imported_non_destructive: false,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
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
            Some("Freebox OS requires HTTPS for API access; plain HTTP is being removed."),
        ),
        connection_field(
            "api_url",
            "API URL",
            true,
            Some("/api/v1/"),
            Some("Path and version of the Freebox API, for example /api/v1/."),
        ),
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
            ConfigFieldType::Path,
            false,
            None,
            Some("Absolute path on the Freebox. Cannot be combined with Category."),
        ),
        field(
            "category",
            "Category",
            ConfigFieldType::String,
            false,
            None,
            Some("Sub-folder of the Freebox download directory. Letters and - only."),
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
// Operations
// ---------------------------------------------------------------------------

pub fn scryer_download_add(input: String) -> FnResult<String> {
    respond(add(&input))
}

fn add(input: &str) -> Result<PluginDownloadClientAddResponse, PluginError> {
    let request: PluginDownloadClientAddRequest = parse_request(input)?;
    let config = FreeboxConfig::from_host();
    if let Some(problem) = add_settings_problem(&config) {
        return Err(plugin_error(PluginErrorCode::InvalidConfig, problem));
    }

    let directory = download_directory(&config, &request)?;
    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty());

    let added = if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        let decoded = STANDARD.decode(bytes).map_err(|error| {
            detailed_error(
                PluginErrorCode::Permanent,
                "The torrent Scryer supplied was not valid base64.",
                error.to_string(),
            )
        })?;
        add_file(&config, &torrent_file_name(&request), &decoded, &directory)
    } else if let Some(source) = source_url(&request) {
        add_url(&config, &source, &directory)
    } else {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "download source is missing",
        ));
    };

    let id = match added {
        Ok(id) => id,
        // `exists` — "Same task already exists" (dev.freebox.fr/sdk/os/download/,
        // *Download Errors*). The grab succeeded on an earlier attempt; failing
        // here would make Scryer re-grab a release the box is already
        // downloading, so adopt the existing task instead.
        Err(error) if error.is("exists") => {
            match find_existing_task(&config, hash.as_deref(), &directory)? {
                Some(task) => {
                    warn_log!(
                        "Freebox already had this download; adopting task {}",
                        task.id
                    );
                    task.id
                }
                None => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    };

    // A task that exists but could not be configured is still a real download:
    // reporting a failure here would orphan it in the Freebox queue and make
    // Scryer grab the release a second time.
    if let Err(error) = set_torrent_settings(&config, &id, &request) {
        warn_log!(
            "Freebox accepted task {id} but rejected its options: {}",
            error.error.public_message
        );
    }

    Ok(PluginDownloadClientAddResponse {
        client_item_id: id,
        info_hash: hash,
    })
}

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    respond(list_queue())
}

fn list_queue() -> Result<Vec<PluginDownloadItem>, PluginError> {
    let config = FreeboxConfig::from_host();
    Ok(scoped_tasks(&config)?
        .into_iter()
        .map(|task| torrent_to_item(&config, task))
        .collect())
}

/// Freebox keeps no separate failed history: `/downloads/` is the whole task
/// list, errors included, and the bridge already merges this into the queue
/// (`download_client_bridge.rs:160-226`). Returning the list again polled the
/// box twice and produced a duplicate of every item for the merge to dedupe.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    respond(Ok::<Vec<PluginDownloadItem>, PluginError>(Vec::new()))
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    respond(list_completed())
}

fn list_completed() -> Result<Vec<PluginCompletedDownload>, PluginError> {
    let config = FreeboxConfig::from_host();
    Ok(scoped_tasks(&config)?
        .into_iter()
        // `done` *and* `seeding`: both mean the payload is on disk, and Sonarr
        // reports both as `Completed` (`TorrentFreeboxDownload.cs:109-112`).
        // Waiting for `done` alone held every import hostage to the box's
        // `stop_ratio`.
        .filter(|task| matches!(task.status.as_str(), "done" | "seeding"))
        .map(|task| torrent_to_completed(&config, task))
        .collect())
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    respond(control(&input))
}

fn control(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientControlRequest = parse_request(input)?;
    let config = FreeboxConfig::from_host();
    match request.action {
        DownloadControlAction::Remove => {
            let path = if request.remove_data {
                format!("/downloads/{}/erase", request.client_item_id)
            } else {
                format!("/downloads/{}", request.client_item_id)
            };
            match api_unit(&config, "DELETE", &path, RequestBody::None, true) {
                Ok(()) => Ok(()),
                // A task that is already gone is the outcome the caller wanted.
                Err(error) if error.is("task_not_found") => {
                    warn_log!(
                        "Freebox has no task {}; treating the removal as done",
                        request.client_item_id
                    );
                    Ok(())
                }
                Err(error) => Err(error.into()),
            }
        }
        DownloadControlAction::Pause => set_status(&config, &request.client_item_id, "stopped"),
        DownloadControlAction::Resume => {
            set_status(&config, &request.client_item_id, "downloading")
        }
        // Freebox has an `io_priority` and a queue position, but nothing that
        // starts a task outside its download slots.
        DownloadControlAction::ForceStart => Err(plugin_error(
            PluginErrorCode::Unsupported,
            "Freebox cannot force-start a download task.",
        )),
    }
}

fn set_status(config: &FreeboxConfig, id: &str, status: &str) -> Result<(), PluginError> {
    api_unit(
        config,
        "PUT",
        &format!("/downloads/{id}"),
        RequestBody::Json(serde_json::json!({ "status": status })),
        true,
    )
    .map_err(PluginError::from)
}

/// Freebox OS has no tag, label, category or view, so there is nothing to write
/// back after an import and nothing a destructive mark could do that the core's
/// seeding gate does not already own (`00-common.md` rule 3).
///
/// The descriptor says so (`mark_imported_non_destructive: false`), the
/// function table leaves the non-destructive slot empty so the bridge answers
/// `Ok(())` itself, and this body exists only because the legacy table requires
/// the destructive slot to be filled. Removing a finished torrent at import
/// time would cut its seeding short and is never the plugin's decision.
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let _request: PluginDownloadClientMarkImportedRequest = match parse_request(&input) {
        Ok(request) => request,
        Err(error) => return respond(Err::<(), PluginError>(error)),
    };
    respond(Ok::<(), PluginError>(()))
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    respond(client_status())
}

fn client_status() -> Result<PluginDownloadClientStatus, PluginError> {
    let config = FreeboxConfig::from_host();
    let mut warnings = Vec::new();

    // `/api_version` is unauthenticated and lives on the HTTP root; a box that
    // does not answer it is not a reason to fail the whole status.
    let discovery = match api_version(&config) {
        Ok(discovery) => Some(discovery),
        Err(error) => {
            warn_log!(
                "Freebox /api_version was unreadable: {}",
                error.public_message
            );
            None
        }
    };
    if let Some(discovery) = discovery.as_ref()
        && let Some(problem) = api_version_problem(&config, discovery)
    {
        warnings.push(problem);
    }
    if config.scheme == "http" {
        warnings.push(
            "Freebox OS requires HTTPS for API access and plain HTTP is being removed; enable Use SSL."
                .to_string(),
        );
    }

    let root = configured_download_root(&config)?;
    Ok(PluginDownloadClientStatus {
        version: discovery
            .as_ref()
            .map(|discovery| discovery.api_version.clone())
            .filter(|value| !value.is_empty()),
        is_localhost: Some(is_localhost_host(&config.host)),
        remote_output_roots: if root.is_empty() {
            Vec::new()
        } else {
            vec![root]
        },
        // Freebox never drops a task on its own; it stops seeding at
        // `stop_ratio` and keeps the entry. Removal stays the core's call.
        removes_completed_downloads: Some(false),
        sorting_mode: Some("freebox-api".to_string()),
        warnings,
    })
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    respond(test_connection())
}

/// Sonarr's `Test` is `Authenticate(Settings)` with the failure routed to
/// `Host`/`Port`, `AppId`/`AppToken` or `ApiUrl`
/// (`TorrentFreeboxDownload.cs:168-188`). The typed codes carry that
/// distinction here, and the settings validator Sonarr runs separately runs
/// first.
fn test_connection() -> Result<String, PluginError> {
    let config = FreeboxConfig::from_host();
    if let Some(problem) = settings_problem(&config) {
        return Err(plugin_error(PluginErrorCode::InvalidConfig, problem));
    }

    let discovery = api_version(&config).ok();
    if let Some(discovery) = discovery.as_ref()
        && let Some(problem) = api_version_problem(&config, discovery)
    {
        return Err(plugin_error(PluginErrorCode::InvalidConfig, problem));
    }

    // A test must never pass on a cached session token.
    forget_var(SESSION_VAR_KEY);
    authenticate(&config)?;
    // Proves the `downloader` permission end to end, the way Sonarr's other
    // clients follow authentication with a listing.
    list_tasks(&config)?;

    Ok(discovery
        .map(|discovery| discovery.api_version)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ok".to_string()))
}

// ---------------------------------------------------------------------------
// Add helpers
// ---------------------------------------------------------------------------

/// Sonarr's `GetDownloadDirectory(remoteEpisode)`
/// (`TorrentFreeboxDownload.cs:190-215`) always appends the cleaned release
/// title, so the task's `download_dir` *is* the job folder Scryer later imports
/// from. The port sent the bare root, which made every import scan the whole
/// download directory.
fn download_directory(
    config: &FreeboxConfig,
    request: &PluginDownloadClientAddRequest,
) -> Result<String, ApiError> {
    if let Some(directory) = request
        .routing
        .download_directory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        // The core already isolated this one; appending to it would nest a
        // second folder inside a folder that exists for exactly this purpose.
        return Ok(directory.trim_end_matches('/').to_string());
    }
    let root = configured_download_root(config)?;
    Ok(match job_folder_name(request) {
        Some(folder) => format!("{}/{folder}", root.trim_end_matches('/')),
        None => root,
    })
}

fn job_folder_name(request: &PluginDownloadClientAddRequest) -> Option<String> {
    request
        .release
        .release_title
        .as_deref()
        .or(request.source.source_title.as_deref())
        .or(Some(request.title.title_name.as_str()))
        .map(clean_file_name)
        .filter(|value| !value.is_empty())
}

/// The configured destination, else the box's own `download_dir` with the
/// category appended (`TorrentFreeboxDownload.cs:192-206`). This is the output
/// root as well, so it deliberately carries no per-release folder.
fn configured_download_root(config: &FreeboxConfig) -> Result<String, ApiError> {
    if !config.destination_directory.is_empty() {
        return Ok(config
            .destination_directory
            .trim_end_matches('/')
            .to_string());
    }
    let download_config: FreeboxDownloadConfiguration =
        api_result(config, "GET", "/downloads/config/", RequestBody::None, true)?;
    let mut root = decode_base64(&download_config.download_directory)
        .trim_end_matches('/')
        .to_string();
    if !config.category.is_empty() {
        root = format!("{root}/{}", config.category);
    }
    Ok(root)
}

fn add_url(config: &FreeboxConfig, url: &str, directory: &str) -> Result<String, ApiError> {
    let mut form = vec![("download_url".to_string(), url.to_string())];
    if !directory.is_empty() {
        form.push(("download_dir".to_string(), STANDARD.encode(directory)));
    }
    added_task_id(api_result(
        config,
        "POST",
        "/downloads/add",
        RequestBody::Form(&form),
        true,
    )?)
}

fn add_file(
    config: &FreeboxConfig,
    file_name: &str,
    file_bytes: &[u8],
    directory: &str,
) -> Result<String, ApiError> {
    let form = if directory.is_empty() {
        Vec::new()
    } else {
        vec![("download_dir".to_string(), STANDARD.encode(directory))]
    };
    added_task_id(api_result(
        config,
        "POST",
        "/downloads/add",
        RequestBody::Multipart {
            fields: &form,
            file_name,
            bytes: file_bytes,
        },
        true,
    )?)
}

/// `POST /downloads/add` answers `{"result": {"id": 23}}` — an int, or a list
/// of ints for `download_url_list` (dev.freebox.fr/sdk/os/download/, *Adding a
/// new Download task*).
fn added_task_id(task: FreeboxDownloadTask) -> Result<String, ApiError> {
    if task.id.is_empty() {
        return Err(ApiError::new(plugin_error(
            PluginErrorCode::Temporary,
            "Freebox accepted the download but did not return a task id.",
        )));
    }
    Ok(task.id)
}

/// The task an `exists` refers to: matched on the info hash the indexer gave
/// us, else on the per-release job folder, which is unique by construction.
fn find_existing_task(
    config: &FreeboxConfig,
    info_hash: Option<&str>,
    directory: &str,
) -> Result<Option<FreeboxDownloadTask>, PluginError> {
    let tasks = list_tasks(config)?;
    Ok(tasks.into_iter().find(|task| {
        info_hash.is_some_and(|hash| normalize_hash(&task.info_hash) == hash)
            || (!directory.is_empty()
                && decode_base64(&task.download_directory).trim_end_matches('/') == directory)
    }))
}

fn set_torrent_settings(
    config: &FreeboxConfig,
    id: &str,
    request: &PluginDownloadClientAddRequest,
) -> Result<(), ApiError> {
    let Some(body) = torrent_settings_body(config, request) else {
        return Ok(());
    };
    api_unit(
        config,
        "PUT",
        &format!("/downloads/{id}"),
        RequestBody::Json(serde_json::Value::Object(body)),
        true,
    )
}

/// `SetTorrentSettings` (`FreeboxDownloadProxy.cs:121-153`), with two
/// corrections from the documentation: `queue_pos` is an `int`, not the string
/// Sonarr sends, and `stop_ratio` is an integer percentage (`150` = 1.5) rather
/// than the fractional ratio the port used to send.
fn torrent_settings_body(
    config: &FreeboxConfig,
    request: &PluginDownloadClientAddRequest,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut body = serde_json::Map::new();
    if request_paused(config, request) {
        body.insert(
            "status".to_string(),
            serde_json::Value::String("stopped".to_string()),
        );
    }
    if queue_first(config, request) {
        body.insert("queue_pos".to_string(), serde_json::json!(1));
    }
    if let Some(ratio) = seed_ratio(request) {
        // 0 means unlimited seeding.
        body.insert("stop_ratio".to_string(), serde_json::json!(ratio));
    }
    (!body.is_empty()).then_some(body)
}

fn request_paused(config: &FreeboxConfig, request: &PluginDownloadClientAddRequest) -> bool {
    match request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.initial_state)
    {
        Some(PluginTorrentInitialState::Paused) | Some(PluginTorrentInitialState::Stopped) => true,
        Some(PluginTorrentInitialState::Started) => false,
        Some(PluginTorrentInitialState::Default) | None => config.add_paused,
    }
}

/// `ToBeQueuedFirst` (`TorrentFreeboxDownload.cs:222-231`), except that an
/// explicit `queue_placement` from the core wins over the configured priority.
fn queue_first(config: &FreeboxConfig, request: &PluginDownloadClientAddRequest) -> bool {
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

/// `GetSeedRatio` (`TorrentFreeboxDownload.cs:233-241`): the ratio scaled by
/// 100, which is what `stop_ratio` is documented to be.
fn seed_ratio(request: &PluginDownloadClientAddRequest) -> Option<i64> {
    request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.seed_goal_ratio)
        .or(request.release.seed_goal_ratio)
        .map(|ratio| (ratio * 100.0).round().max(0.0) as i64)
}

/// The uploaded file name, which is what Freebox shows while the metadata is
/// still being fetched. `download.torrent` made every upload look the same.
fn torrent_file_name(request: &PluginDownloadClientAddRequest) -> String {
    let base = request
        .source
        .torrent_file_name
        .as_deref()
        .map(str::to_string)
        .or_else(|| job_folder_name(request))
        .unwrap_or_else(|| "download".to_string());
    if base.to_ascii_lowercase().ends_with(".torrent") {
        base
    } else {
        format!("{base}.torrent")
    }
}

// ---------------------------------------------------------------------------
// Listing helpers
// ---------------------------------------------------------------------------

fn list_tasks(config: &FreeboxConfig) -> Result<Vec<FreeboxDownloadTask>, PluginError> {
    let tasks: Vec<FreeboxDownloadTask> =
        api_result(config, "GET", "/downloads/", RequestBody::None, true)?;
    Ok(tasks)
}

fn scoped_tasks(config: &FreeboxConfig) -> Result<Vec<FreeboxDownloadTask>, PluginError> {
    Ok(list_tasks(config)?
        .into_iter()
        // `GetTorrents` keeps only `bt` tasks (`TorrentFreeboxDownload.cs:40-43`);
        // the same box also runs http/ftp/nzb downloads.
        .filter(|task| task.task_type.eq_ignore_ascii_case("bt"))
        .filter(|task| matches_scope(config, task))
        .collect())
}

/// `GetItems`' scope filter (`TorrentFreeboxDownload.cs:53-71`).
///
/// Sonarr compares whole path segments (`OsPath.Contains`,
/// `OsPath.cs:344-370`); the port used `str::starts_with`, so a destination of
/// `/downloads` also swallowed `/downloads-old`.
fn matches_scope(config: &FreeboxConfig, task: &FreeboxDownloadTask) -> bool {
    let output = decode_base64(&task.download_directory);
    // The destination wins, exactly as it does in `configured_download_root`:
    // with both set, the task lands under the destination without a category
    // segment, so demanding the segment here would hide the plugin's own adds.
    if !config.destination_directory.is_empty() {
        return path_contains(&config.destination_directory, &output);
    }
    if !config.category.is_empty() {
        return matched_category(config, &output).is_some();
    }
    true
}

fn path_contains(base: &str, candidate: &str) -> bool {
    let base = path_segments(base);
    let candidate = path_segments(candidate);
    candidate.len() >= base.len()
        && base
            .iter()
            .zip(candidate)
            .all(|(left, right)| *left == right)
}

fn path_segments(value: &str) -> Vec<&str> {
    value
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// The category as the *box* spells it, matched case-insensitively
/// (`00-common.md` rule 5). Sonarr compares and reports the configured string.
fn matched_category(config: &FreeboxConfig, output: &str) -> Option<String> {
    if config.category.is_empty() {
        return None;
    }
    path_segments(output)
        .into_iter()
        .find(|segment| segment.eq_ignore_ascii_case(&config.category))
        .map(str::to_string)
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
        message: item_message(&torrent),
        category: matched_category(config, &output),
        remote_output_path: non_empty(output.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: non_empty(normalize_hash(&torrent.info_hash)),
            save_path: non_empty(output.clone()),
            content_paths: non_empty(output).into_iter().collect(),
            uploaded_bytes: Some(torrent.transmitted_bytes),
            downloaded_bytes: Some(torrent.received_bytes),
            upload_rate_bytes_per_second: Some(torrent.transmitted_rate),
            download_rate_bytes_per_second: Some(torrent.received_rate),
            // The *observed* ratio. This used to report `stop_ratio`, i.e. the configured
            // goal, which made every torrent look like it had already met its target.
            seed_ratio: observed_ratio(&torrent),
            raw_status: Some(torrent.status.clone()),
            status_reason: task_error(&torrent).map(str::to_string),
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
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files: Some(is_data_complete(&torrent)),
        can_remove: derive_can_remove(&torrent),
        removed: Some(false),
        raw_state: Some(torrent.status),
        // Freebox reports `created_ts` and nothing else; there is no completion
        // timestamp to hand over, and inventing one from the creation time
        // would be worse than saying nothing.
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
        category: matched_category(config, &output),
        // `download_dir` is the directory the task downloads into — the
        // per-release job folder — never a file, whatever its name looks like.
        output_kind: Some(PluginDownloadOutputKind::Directory),
        content_paths: non_empty(output).into_iter().collect(),
        size_bytes: Some(torrent.size),
        completed_at: None,
        parameters: Vec::new(),
        release_name: None,
    }
}

/// Ratio actually achieved, from the task's transfer counters.
fn observed_ratio(torrent: &FreeboxDownloadTask) -> Option<f64> {
    (torrent.received_bytes > 0)
        .then(|| torrent.transmitted_bytes as f64 / torrent.received_bytes as f64)
}

/// Whether the payload is fully downloaded and stable on disk.
///
/// `checking`, `repairing` and `extracting` are post-download phases that
/// rewrite the files (dev.freebox.fr/sdk/os/download/, *status*), so a complete
/// byte count does not make them movable.
fn is_data_complete(torrent: &FreeboxDownloadTask) -> bool {
    match torrent.status.as_str() {
        "done" | "seeding" => true,
        "checking" | "repairing" | "extracting" | "error" => false,
        _ => torrent.received_percent >= 10_000,
    }
}

/// Honest `can_remove` for the Freebox download manager.
///
/// Freebox reports `done` only once it has stopped seeding a torrent, and `seeding` while the
/// torrent is still being served toward its `stop_ratio` (a percentage; `<= 0` means "no
/// limit"). Anything else — user-stopped, errored, still downloading — is either unfinished
/// or unknowable.
fn derive_can_remove(torrent: &FreeboxDownloadTask) -> Option<bool> {
    match torrent.status.as_str() {
        // Freebox finished with the torrent: it is no longer seeding.
        "done" => Some(true),
        "seeding" => {
            if torrent.stop_ratio <= 0 {
                // No client-side seeding goal; Scryer-side evaluation decides.
                return None;
            }
            let goal = torrent.stop_ratio as f64 / 100.0;
            match observed_ratio(torrent) {
                // Goal reached but Freebox has not switched the task to `done` yet.
                Some(ratio) if ratio >= goal => None,
                Some(_) => Some(false),
                None => None,
            }
        }
        // Stopped/queued/errored complete torrents are not seeding for a reason Freebox
        // attributes to a limit, so the seeding verdict is unknowable.
        _ if is_data_complete(torrent) => None,
        _ => Some(false),
    }
}

/// The documented task statuses (dev.freebox.fr/sdk/os/download/, *status*).
///
/// Sonarr collapses `checking` into `Downloading` and `seeding` into
/// `Completed` (`TorrentFreeboxDownload.cs:86-121`) because its
/// `DownloadItemStatus` has nothing better. Scryer's does: `Verifying`,
/// `Repairing`, `Extracting` and `Seeding` are all in the contract, and the
/// core maps `Seeding` to the completed queue state anyway
/// (`download_client_adapter.rs:354`). An unrecognised status keeps polling as
/// `Downloading` (`00-common.md` rule 2).
fn map_state(torrent: &FreeboxDownloadTask) -> DownloadItemState {
    match torrent.status.as_str() {
        "stopped" | "stopping" => DownloadItemState::Paused,
        "queued" => DownloadItemState::Queued,
        "starting" | "downloading" | "retry" => DownloadItemState::Downloading,
        "checking" => DownloadItemState::Verifying,
        "repairing" => DownloadItemState::Repairing,
        "extracting" => DownloadItemState::Extracting,
        "error" => DownloadItemState::Warning,
        "done" => DownloadItemState::Completed,
        "seeding" => DownloadItemState::Seeding,
        _ => DownloadItemState::Downloading,
    }
}

fn item_message(torrent: &FreeboxDownloadTask) -> Option<String> {
    if torrent.status == "error" {
        return Some(error_description(&torrent.error));
    }
    // Sonarr's `UnknownDownloadState` message (`TorrentFreeboxDownload.cs:114-120`).
    if !is_known_status(&torrent.status) {
        return Some(format!("Unknown download state: {}", torrent.status));
    }
    None
}

fn is_known_status(status: &str) -> bool {
    matches!(
        status,
        "stopped"
            | "stopping"
            | "queued"
            | "starting"
            | "downloading"
            | "retry"
            | "checking"
            | "repairing"
            | "extracting"
            | "error"
            | "done"
            | "seeding"
    )
}

/// The task `error` field, with the documented `none` ("No error") treated as
/// what it is. The port reported `none` as a status reason on every healthy
/// task and described it as "none - Unknown error".
fn task_error(torrent: &FreeboxDownloadTask) -> Option<&str> {
    match torrent.error.as_str() {
        "" | "none" => None,
        error => Some(error),
    }
}

/// `FreeboxDownloadTask.GetErrorDescription` (`FreeboxDownloadTask.cs:96-135`),
/// extended with the `none`, `http_*` and `http_redirections_exceeded` values
/// the documentation lists and Sonarr's table omits.
fn error_description(error: &str) -> String {
    match error {
        "" | "none" => "No error.".to_string(),
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
        "http_redirections_exceeded" => "Too many HTTP redirections.".to_string(),
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
        value => match value.strip_prefix("http_") {
            Some(code) => format!("HTTP {code} error."),
            None => format!("{value} - Unknown error"),
        },
    }
}

// ---------------------------------------------------------------------------
// Freebox API transport
// ---------------------------------------------------------------------------

enum RequestBody<'a> {
    None,
    Json(serde_json::Value),
    Form(&'a [(String, String)]),
    Multipart {
        fields: &'a [(String, String)],
        file_name: &'a str,
        bytes: &'a [u8],
    },
}

fn api_result<T: DeserializeOwned>(
    config: &FreeboxConfig,
    method: &str,
    path: &str,
    body: RequestBody<'_>,
    auth: bool,
) -> Result<T, ApiError> {
    let value = api_envelope(config, method, path, body, auth)?.ok_or_else(|| {
        ApiError::new(plugin_error(
            PluginErrorCode::Temporary,
            "The Freebox API answered without a result.",
        ))
    })?;
    serde_json::from_value(value).map_err(|error| {
        ApiError::new(detailed_error(
            PluginErrorCode::Temporary,
            "The Freebox API answered with a result this plugin could not read.",
            error.to_string(),
        ))
    })
}

fn api_unit(
    config: &FreeboxConfig,
    method: &str,
    path: &str,
    body: RequestBody<'_>,
    auth: bool,
) -> Result<(), ApiError> {
    // `DELETE /downloads/{id}` answers `{"success": true}` with no `result` at
    // all, which the port treated as a failure.
    api_envelope(config, method, path, body, auth).map(|_| ())
}

/// One request, with a single re-authentication when the box says the session
/// token has expired.
///
/// The documentation is explicit that this happens by design: "The validity of
/// the session_token is limited in time and the app will have to renew this
/// session_token once in a while" (dev.freebox.fr/sdk/os/login/). Sonarr only
/// drops the cached token and fails the call.
fn api_envelope(
    config: &FreeboxConfig,
    method: &str,
    path: &str,
    body: RequestBody<'_>,
    auth: bool,
) -> Result<Option<serde_json::Value>, ApiError> {
    let mut renewed = false;
    loop {
        let token = if auth {
            Some(session_token(config)?)
        } else {
            None
        };
        let response = send(config, method, path, &body, token.as_deref())?;
        match interpret(&response) {
            Ok(result) => return Ok(result),
            Err(error) => {
                if auth && error.error.code == PluginErrorCode::AuthFailed {
                    // The cached token is worthless either way.
                    forget_var(SESSION_VAR_KEY);
                    if !renewed && is_session_expired(&error) {
                        renewed = true;
                        continue;
                    }
                }
                return Err(error);
            }
        }
    }
}

fn is_session_expired(error: &ApiError) -> bool {
    error.is("auth_required")
}

struct RawResponse {
    status: u16,
    location: Option<String>,
    body: String,
}

fn send(
    config: &FreeboxConfig,
    method: &str,
    path: &str,
    body: &RequestBody<'_>,
    token: Option<&str>,
) -> Result<RawResponse, ApiError> {
    let url = format!(
        "{}{}{}",
        config.api_root.trim_end_matches('/'),
        if path.starts_with('/') { "" } else { "/" },
        path
    );
    let mut request = HttpRequest::new(url)
        .with_method(method)
        .with_header("User-Agent", USER_AGENT)
        .with_header("Accept", "application/json");
    if let Some(token) = token {
        request = request.with_header("X-Fbx-App-Auth", token);
    }

    let payload = match body {
        RequestBody::None => {
            request = request.with_header("Content-Type", "application/json");
            None
        }
        RequestBody::Json(value) => {
            request = request.with_header("Content-Type", "application/json");
            Some(serde_json::to_vec(value).map_err(|error| {
                ApiError::new(detailed_error(
                    PluginErrorCode::Permanent,
                    "Failed to encode a Freebox API request.",
                    error.to_string(),
                ))
            })?)
        }
        // "for this API the request arguments must be encoded using
        // application/x-www-form-urlencoded (or multipart/form-data for file
        // upload) instead of application/json"
        // (dev.freebox.fr/sdk/os/download/, *Adding a new Download task*).
        RequestBody::Form(form) => {
            request = request.with_header("Content-Type", "application/x-www-form-urlencoded");
            Some(encode_form(form).into_bytes())
        }
        RequestBody::Multipart {
            fields,
            file_name,
            bytes,
        } => {
            let boundary = "scryer-freebox-boundary";
            request = request.with_header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            );
            Some(encode_multipart(boundary, fields, file_name, bytes))
        }
    };

    let response = http::request::<Vec<u8>>(&request, payload)
        .map_err(|error| ApiError::new(classify_transport_error(&error.to_string())))?;
    Ok(RawResponse {
        status: response.status_code(),
        location: header_value(&response, "Location"),
        body: String::from_utf8_lossy(&response.body()).to_string(),
    })
}

/// `ProcessRequest` (`FreeboxDownloadProxy.cs:222-275`), with typed codes.
fn interpret(response: &RawResponse) -> Result<Option<serde_json::Value>, ApiError> {
    let envelope: Option<FreeboxResponse> = serde_json::from_str(&response.body).ok();

    match response.status {
        200..=299 => {
            let Some(envelope) = envelope else {
                return Err(ApiError::new(detailed_error(
                    PluginErrorCode::InvalidConfig,
                    "The configured URL did not answer with a Freebox API response; verify 'API URL'.",
                    truncate(&response.body),
                )));
            };
            if envelope.success {
                Ok(envelope.result)
            } else {
                Err(api_failure(&envelope))
            }
        }
        // The host runs plugin HTTP without following redirects, so a captive
        // portal or a reverse proxy in front of the box shows up here rather
        // than as an unreadable body.
        300..=399 => Err(ApiError::new(plugin_error(
            PluginErrorCode::InvalidConfig,
            match response.location.as_deref() {
                Some(location) => format!(
                    "The Freebox API redirected to {location}; verify 'Host', 'Port' and 'API URL'."
                ),
                None => {
                    "The Freebox API redirected the request; verify 'Host', 'Port' and 'API URL'."
                        .to_string()
                }
            },
        ))),
        // "in case of [an authentication error] the HTTP 403 return code will
        // be used as well" (dev.freebox.fr/sdk/os/login/).
        401 | 403 => {
            let mut failure = envelope.as_ref().map(api_failure).unwrap_or_else(|| {
                ApiError::new(plugin_error(
                    PluginErrorCode::AuthFailed,
                    "Authentication to the Freebox API failed; verify 'App ID' and 'App Token'.",
                ))
            });
            // Whatever the body says, the box refused the credentials
            // (`FreeboxDownloadProxy.cs:242-251`).
            failure.error.code = PluginErrorCode::AuthFailed;
            Err(failure)
        }
        404 => Err(ApiError::new(plugin_error(
            PluginErrorCode::InvalidConfig,
            "Unable to reach Freebox API. Verify 'API URL' setting for base URL and version.",
        ))),
        500..=599 => Err(ApiError::new(detailed_error(
            PluginErrorCode::Temporary,
            format!("The Freebox API returned HTTP {}.", response.status),
            truncate(&response.body),
        ))),
        status => Err(envelope.as_ref().map(api_failure).unwrap_or_else(|| {
            ApiError::new(detailed_error(
                PluginErrorCode::Permanent,
                format!("The Freebox API returned HTTP {status}."),
                truncate(&response.body),
            ))
        })),
    }
}

fn api_failure(envelope: &FreeboxResponse) -> ApiError {
    let code = envelope.error_code.as_deref().unwrap_or_default();
    let description = api_error_description(code);
    let message = match envelope.msg.as_deref().map(str::trim) {
        Some(msg) if !msg.is_empty() && description.is_empty() => msg.to_string(),
        _ if description.is_empty() => format!("{code} - Unknown error"),
        _ => description.to_string(),
    };
    if code.is_empty() {
        return ApiError::new(plugin_error(
            PluginErrorCode::Temporary,
            format!("The Freebox API returned an error: {message}"),
        ));
    }
    ApiError::with_code(
        code,
        detailed_error(
            api_error_code(code),
            format!("The Freebox API returned an error: {message}"),
            code,
        ),
    )
}

/// `FreeboxResponse.Descriptions` (`FreeboxResponse.cs:19-57`), which is the
/// union of the *Authentication errors* table (dev.freebox.fr/sdk/os/login/)
/// and the *Download Errors* table (dev.freebox.fr/sdk/os/download/).
fn api_error_description(code: &str) -> &'static str {
    match code {
        // Common
        "invalid_request" => "Your request is invalid.",
        "invalid_api_version" => "Invalid API base url or unknown API version.",
        "internal_error" => "Internal error.",
        // Login
        "auth_required" => "Invalid session token, or no session token sent.",
        "invalid_token" => "The app token you are trying to use is invalid or has been revoked.",
        "pending_token" => {
            "The app token you are trying to use has not been validated by user yet."
        }
        "insufficient_rights" => "Your app permissions does not allow accessing this API.",
        "denied_from_external_ip" => "You are trying to get an app_token from a remote IP.",
        "ratelimited" => "Too many auth error have been made from your IP.",
        "new_apps_denied" => "New application token request has been disabled.",
        "apps_denied" => "API access from apps has been disabled.",
        // Download
        "task_not_found" => "No task was found with the given id.",
        "invalid_operation" => "Attempt to perform an invalid operation.",
        "invalid_file" => "Error with the download file (invalid format ?).",
        "invalid_url" => "URL is invalid.",
        "not_implemented" => "Method not implemented.",
        "out_of_memory" => "No more memory available to perform the requested action.",
        "invalid_task_type" => "The task type is invalid.",
        "hibernating" => "The downloader is hibernating.",
        "need_bt_stopped_done" => {
            "This action is only valid for Bittorrent task in stopped or done state."
        }
        "bt_tracker_not_found" => "Attempt to access an invalid tracker object.",
        "too_many_tasks" => "Too many tasks.",
        "invalid_address" => "Invalid peer address.",
        "port_conflict" => "Port conflict when setting config.",
        "invalid_priority" => "Invalid priority.",
        "ctx_file_error" => "Failed to initialize task context file (need to check disk).",
        "exists" => "Same task already exists.",
        "port_outside_range" => "Incoming port is not available for this customer.",
        _ => "",
    }
}

/// Which of Scryer's codes each Freebox `error_code` is (`00-common.md` rule 4).
fn api_error_code(code: &str) -> PluginErrorCode {
    match code {
        "auth_required"
        | "invalid_token"
        | "pending_token"
        | "insufficient_rights"
        | "denied_from_external_ip"
        | "new_apps_denied"
        | "apps_denied" => PluginErrorCode::AuthFailed,
        "ratelimited" => PluginErrorCode::RateLimited,
        "invalid_api_version" | "port_conflict" | "port_outside_range" => {
            PluginErrorCode::InvalidConfig
        }
        // Transient box conditions: the same grab can succeed on the next pass.
        "too_many_tasks" | "hibernating" | "out_of_memory" | "internal_error"
        | "ctx_file_error" => PluginErrorCode::Temporary,
        "not_implemented" => PluginErrorCode::Unsupported,
        // Everything else is a statement about the request itself.
        _ => PluginErrorCode::Permanent,
    }
}

fn classify_transport_error(detail: &str) -> PluginError {
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("timeout") || lowered.contains("timed out") {
        detailed_error(
            PluginErrorCode::Temporary,
            "The Freebox API did not answer in time.",
            detail,
        )
    } else if lowered.contains("certificate")
        || lowered.contains("tls")
        || lowered.contains("ssl")
        || lowered.contains("trust")
    {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to reach the Freebox API: certificate validation failed. The Freebox uses its own root CA.",
            detail,
        )
    } else {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to reach Freebox API. Verify 'Host', 'Port' or 'Use SSL' settings.",
            detail,
        )
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

fn session_token(config: &FreeboxConfig) -> Result<String, ApiError> {
    if let Some(token) = var::get(SESSION_VAR_KEY)
        .ok()
        .flatten()
        .map(|value: String| value)
        .filter(|value| !value.is_empty())
    {
        return Ok(token);
    }
    authenticate(config).map_err(ApiError::new)
}

/// `GetSessionToken` (`FreeboxDownloadProxy.cs:155-203`): `GET /login` for the
/// challenge, `password = hmac-sha1(app_token, challenge)` in lower-case hex,
/// then `POST /login/session` with `app_id` and that password
/// (dev.freebox.fr/sdk/os/login/).
fn authenticate(config: &FreeboxConfig) -> Result<String, PluginError> {
    let challenge: FreeboxLogin = api_result(config, "GET", "/login", RequestBody::None, false)?;
    if challenge.challenge.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "The Freebox API did not return a login challenge; verify 'API URL'.",
        ));
    }
    let mut mac = Hmac::<Sha1>::new_from_slice(config.app_token.as_bytes()).map_err(|error| {
        detailed_error(
            PluginErrorCode::InvalidConfig,
            "The configured 'App Token' cannot be used to sign the login challenge.",
            error.to_string(),
        )
    })?;
    mac.update(challenge.challenge.as_bytes());
    let password = hex_lower(&mac.finalize().into_bytes());

    let session: FreeboxLogin = api_result(
        config,
        "POST",
        "/login/session",
        RequestBody::Json(serde_json::json!({
            "app_id": config.app_id,
            "password": password,
        })),
        false,
    )?;
    if session.session_token.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::AuthFailed,
            "The Freebox API did not return a session token; verify 'App ID' and 'App Token'.",
        ));
    }
    // "A permission not listed in app permissions is equivalent to having this
    // permission set to false" (dev.freebox.fr/sdk/os/login/). Sonarr never
    // looks, so a token without the downloader right fails later as an opaque
    // `insufficient_rights` on the first listing.
    if let Some(problem) = permission_problem(session.permissions.as_ref()) {
        return Err(plugin_error(PluginErrorCode::AuthFailed, problem));
    }
    let _ = var::set(SESSION_VAR_KEY, session.session_token.clone());
    Ok(session.session_token)
}

fn permission_problem(
    permissions: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let permissions = permissions?;
    if permissions
        .get("downloader")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    Some(
        "This Freebox application is not allowed to use the downloader; grant it the 'Downloader' permission in Freebox OS."
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// API version discovery
// ---------------------------------------------------------------------------

/// `GET {scheme}://{host}:{port}/api_version` — on the HTTP root, outside the
/// API base and outside the `APIResponse` envelope
/// (dev.freebox.fr/sdk/os/, *Api Discovery using HTTP*).
fn api_version(config: &FreeboxConfig) -> Result<FreeboxApiVersion, PluginError> {
    let request = HttpRequest::new(format!("{}/api_version", config.root_url()))
        .with_method("GET")
        .with_header("User-Agent", USER_AGENT)
        .with_header("Accept", "application/json");
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| classify_transport_error(&error.to_string()))?;
    let body = String::from_utf8_lossy(&response.body()).to_string();
    if response.status_code() >= 400 {
        return Err(detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            format!(
                "The Freebox returned HTTP {} for /api_version.",
                response.status_code()
            ),
            truncate(&body),
        ));
    }
    serde_json::from_str(&body).map_err(|error| {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "The configured host did not answer /api_version like a Freebox.",
            format!("{error}: {}", truncate(&body)),
        )
    })
}

/// The box only serves API versions it knows about: "Other API will be
/// maintained for at least 1 Freebox release" (dev.freebox.fr/sdk/os/). A
/// configured version above the box's own major is a configuration error worth
/// naming before the user hits an opaque 404.
fn api_version_problem(config: &FreeboxConfig, discovery: &FreeboxApiVersion) -> Option<String> {
    let configured = configured_api_major(&config.api_url)?;
    let reported = discovery
        .api_version
        .split('.')
        .next()?
        .trim()
        .parse::<u32>()
        .ok()?;
    let base = match discovery.api_base_url.trim_matches('/') {
        "" => "api",
        base => base,
    };
    (configured > reported).then(|| {
        format!(
            "'API URL' asks for API v{configured} but this Freebox reports API {}; use /{base}/v{reported}/.",
            discovery.api_version,
        )
    })
}

fn configured_api_major(api_url: &str) -> Option<u32> {
    api_url
        .split('/')
        .filter_map(|segment| segment.strip_prefix('v').or(segment.strip_prefix('V')))
        .find_map(|segment| segment.trim().parse::<u32>().ok())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn encode_form(form: &[(String, String)]) -> String {
    form.iter()
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

fn encode_multipart(
    boundary: &str,
    fields: &[(String, String)],
    file_name: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (key, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"download_file\"; filename=\"{}\"\r\n",
            file_name.replace(['"', '\r', '\n'], "")
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/x-bittorrent\r\n\r\n");
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn header_value(response: &HttpResponse, name: &str) -> Option<String> {
    response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
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

/// `FileNameBuilder.CleanFileName` with `NamingConfig.Default`
/// (`FileNameBuilder.cs:1159-1205`): smart colon replacement, then the
/// bad/good character map, then the same trims.
fn clean_file_name(value: &str) -> String {
    let cleaned = value
        .replace(": ", " - ")
        .replace(':', "-")
        .chars()
        .filter_map(|ch| match ch {
            '\\' | '/' => Some('+'),
            '?' => Some('!'),
            '*' => Some('-'),
            '<' | '>' | '|' | '"' => None,
            ch if ch.is_control() => None,
            ch => Some(ch),
        })
        .collect::<String>();
    cleaned
        .trim_start_matches([' ', '.'])
        .trim_end_matches(' ')
        .to_string()
}

fn truncate(value: &str) -> String {
    const LIMIT: usize = 512;
    match value.char_indices().nth(LIMIT) {
        Some((index, _)) => format!("{}…", &value[..index]),
        None => value.to_string(),
    }
}

fn forget_var(key: &str) {
    let _ = var::remove(key);
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

fn nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// Task ids are `int` in the API and `string` in Sonarr's model; accept both.
fn flexible_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Number(i64),
        Text(String),
    }

    Ok(match Option::<Id>::deserialize(deserializer)? {
        Some(Id::Number(number)) => number.to_string(),
        Some(Id::Text(text)) => text,
        None => String::new(),
    })
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

/// `IsLocalhost` (`TorrentFreeboxDownload.cs:163`), which compares the host,
/// not the URL.
fn is_localhost_host(host: &str) -> bool {
    matches!(
        host.trim().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: &str) -> FreeboxDownloadTask {
        FreeboxDownloadTask {
            id: "7".to_string(),
            name: "Movie".to_string(),
            info_hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            status: status.to_string(),
            task_type: "bt".to_string(),
            size: 1_000,
            received_percent: 10_000,
            received_bytes: 1_000,
            transmitted_bytes: 500,
            ..FreeboxDownloadTask::default()
        }
    }

    fn test_config() -> FreeboxConfig {
        FreeboxConfig {
            scheme: "https".to_string(),
            host: "mafreebox.freebox.fr".to_string(),
            port: "443".to_string(),
            api_url: "/api/v1/".to_string(),
            api_root: "https://mafreebox.freebox.fr:443/api/v1".to_string(),
            app_id: "scryer".to_string(),
            app_token: "token".to_string(),
            destination_directory: String::new(),
            category: String::new(),
            recent_priority: PluginTorrentQueuePlacement::Default,
            older_priority: PluginTorrentQueuePlacement::Default,
            add_paused: false,
        }
    }

    /// The fixture's release (`DownloadClientFixtureBase.CreateRemoteEpisode`),
    /// with each section overridable so no test has to repeat the envelope.
    fn request_with(
        release: &str,
        routing: &str,
        torrent: Option<&str>,
    ) -> PluginDownloadClientAddRequest {
        let torrent = match torrent {
            Some(torrent) => format!(r#","torrent":{torrent}"#),
            None => String::new(),
        };
        serde_json::from_str(&format!(
            r#"{{
                "source":{{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn:btih:abc"}},
                "release":{release},
                "title":{{"title_name":"Droned","media_facet":"series","tags":[]}},
                "routing":{routing}{torrent}
            }}"#
        ))
        .expect("an add request")
    }

    fn add_request() -> PluginDownloadClientAddRequest {
        request_with(
            r#"{"release_title":"Droned.S01E01.Pilot.1080p.WEB-DL-DRONE"}"#,
            "{}",
            None,
        )
    }

    fn descriptor() -> serde_json::Value {
        serde_json::from_str(&scryer_describe(String::new()).expect("describe"))
            .expect("descriptor json")
    }

    // -----------------------------------------------------------------------
    // Seeding audit (kept)
    // -----------------------------------------------------------------------

    #[test]
    fn observed_ratio_is_transferred_over_received_not_the_goal() {
        let torrent = FreeboxDownloadTask {
            stop_ratio: 300,
            transmitted_bytes: 500,
            received_bytes: 1_000,
            ..task("seeding")
        };
        assert_eq!(observed_ratio(&torrent), Some(0.5));
    }

    #[test]
    fn can_remove_is_false_while_downloading() {
        let torrent = FreeboxDownloadTask {
            received_percent: 4_000,
            ..task("downloading")
        };
        assert_eq!(derive_can_remove(&torrent), Some(false));
        assert!(!is_data_complete(&torrent));
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_stop_ratio() {
        let torrent = FreeboxDownloadTask {
            stop_ratio: 200,
            ..task("seeding")
        };
        assert_eq!(derive_can_remove(&torrent), Some(false));
    }

    #[test]
    fn can_remove_is_true_once_freebox_marks_the_task_done() {
        assert_eq!(derive_can_remove(&task("done")), Some(true));
    }

    #[test]
    fn can_remove_is_unknown_when_seeding_without_a_stop_ratio() {
        let torrent = FreeboxDownloadTask {
            stop_ratio: 0,
            ..task("seeding")
        };
        assert_eq!(derive_can_remove(&torrent), None);
    }

    #[test]
    fn can_remove_is_unknown_for_a_user_stopped_complete_task() {
        assert_eq!(derive_can_remove(&task("stopped")), None);
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let torrent = FreeboxDownloadTask {
            stop_ratio: 900,
            ..task("seeding")
        };
        let item = torrent_to_item(&test_config(), torrent);
        assert_eq!(item.can_move_files, Some(true));
        assert_eq!(item.can_remove, Some(false));
    }

    #[test]
    fn a_post_download_phase_is_not_movable_data() {
        // `checking`/`repairing`/`extracting` rewrite the files even at 100%.
        for status in ["checking", "repairing", "extracting"] {
            assert!(!is_data_complete(&task(status)), "{status}");
        }
    }

    #[test]
    fn is_private_is_never_claimed_because_freebox_does_not_report_it() {
        let item = torrent_to_item(&test_config(), task("done"));
        assert_eq!(item.torrent.unwrap().is_private, None);
    }

    // -----------------------------------------------------------------------
    // Wire decoding
    // -----------------------------------------------------------------------

    #[test]
    fn a_task_id_decodes_from_the_integer_the_api_documents() {
        // dev.freebox.fr/sdk/os/download/: `id int`, and every example is
        // unquoted. Sonarr's `string Id` only works because Newtonsoft coerces.
        let task: FreeboxDownloadTask =
            serde_json::from_str(r#"{"id":1273,"type":"bt","status":"downloading"}"#)
                .expect("integer id");
        assert_eq!(task.id, "1273");

        let quoted: FreeboxDownloadTask =
            serde_json::from_str(r#"{"id":"1273"}"#).expect("string id");
        assert_eq!(quoted.id, "1273");
    }

    #[test]
    fn an_add_response_decodes_to_the_new_task_id() {
        let envelope: FreeboxResponse =
            serde_json::from_str(r#"{"result":{"id":23},"success":true}"#).expect("envelope");
        let task: FreeboxDownloadTask =
            serde_json::from_value(envelope.result.expect("a result")).expect("task");
        assert_eq!(added_task_id(task).expect("an id"), "23");
    }

    #[test]
    fn a_null_string_field_decodes_as_empty() {
        let task: FreeboxDownloadTask =
            serde_json::from_str(r#"{"id":1,"name":null,"error":null,"info_hash":null}"#)
                .expect("nullable strings");
        assert!(task.name.is_empty() && task.error.is_empty() && task.info_hash.is_empty());
    }

    // -----------------------------------------------------------------------
    // Status mapping (fixture `GetItems_should_return_item_as_downloadItemStatus`)
    // -----------------------------------------------------------------------

    #[test]
    fn the_documented_statuses_map_to_scryer_states() {
        let cases = [
            ("stopped", DownloadItemState::Paused),
            ("stopping", DownloadItemState::Paused),
            ("queued", DownloadItemState::Queued),
            ("starting", DownloadItemState::Downloading),
            ("downloading", DownloadItemState::Downloading),
            ("retry", DownloadItemState::Downloading),
            ("error", DownloadItemState::Warning),
            ("done", DownloadItemState::Completed),
            // Sonarr collapses these four; Scryer's contract carries them.
            ("checking", DownloadItemState::Verifying),
            ("repairing", DownloadItemState::Repairing),
            ("extracting", DownloadItemState::Extracting),
            ("seeding", DownloadItemState::Seeding),
        ];
        for (status, expected) in cases {
            assert_eq!(map_state(&task(status)), expected, "{status}");
        }
    }

    #[test]
    fn an_unknown_status_keeps_polling_and_says_so() {
        let torrent = task("teleporting");
        assert_eq!(map_state(&torrent), DownloadItemState::Downloading);
        assert_eq!(
            item_message(&torrent),
            Some("Unknown download state: teleporting".to_string())
        );
    }

    #[test]
    fn an_errored_task_reports_the_error_description() {
        // Fixture `GetItems_should_return_message_if_tasks_in_error`.
        let torrent = FreeboxDownloadTask {
            error: "internal".to_string(),
            ..task("error")
        };
        let item = torrent_to_item(&test_config(), torrent);
        assert_eq!(item.state, DownloadItemState::Warning);
        assert_eq!(item.message, Some("Internal error.".to_string()));
        assert_eq!(
            item.torrent.expect("torrent").status_reason,
            Some("internal".to_string())
        );
    }

    #[test]
    fn the_documented_none_error_is_not_an_error() {
        let torrent = FreeboxDownloadTask {
            error: "none".to_string(),
            ..task("downloading")
        };
        assert_eq!(item_message(&torrent), None);
        assert_eq!(task_error(&torrent), None);
        assert_eq!(error_description("none"), "No error.");
    }

    #[test]
    fn http_task_errors_are_described_rather_than_called_unknown() {
        assert_eq!(error_description("http_404"), "HTTP 404 error.");
        assert_eq!(error_description("http_5xx"), "HTTP 5xx error.");
        assert_eq!(
            error_description("http_redirections_exceeded"),
            "Too many HTTP redirections."
        );
        assert_eq!(error_description("teapot"), "teapot - Unknown error");
    }

    #[test]
    fn a_decoded_download_directory_is_the_output_path() {
        // Fixture `GetItems_should_return_decoded_destination_directory`.
        let torrent = FreeboxDownloadTask {
            download_directory: "L3RoYXQvdGhlL3BhdGg=".to_string(),
            ..task("done")
        };
        let item = torrent_to_item(&test_config(), torrent);
        assert_eq!(item.remote_output_path, Some("/that/the/path".to_string()));
    }

    // -----------------------------------------------------------------------
    // Scope filter
    // -----------------------------------------------------------------------

    fn task_in(directory: &str) -> FreeboxDownloadTask {
        FreeboxDownloadTask {
            download_directory: STANDARD.encode(directory),
            ..task("done")
        }
    }

    #[test]
    fn a_destination_scope_is_a_path_prefix_not_a_string_prefix() {
        let config = FreeboxConfig {
            destination_directory: "/downloads".to_string(),
            ..test_config()
        };
        assert!(matches_scope(&config, &task_in("/downloads/Show.S01E01")));
        assert!(matches_scope(&config, &task_in("/downloads")));
        // The bug this pins: `/downloads-old` used to match `/downloads`.
        assert!(!matches_scope(&config, &task_in("/downloads-old/Show")));
        // Fixture `GetItems_when_destinationdirectory_is_set_should_ignore_downloads_in_wrong_folder`.
        assert!(!matches_scope(&config, &task_in("/some/path")));
    }

    #[test]
    fn a_category_scope_matches_a_whole_segment_case_insensitively() {
        let config = FreeboxConfig {
            category: "somecat".to_string(),
            ..test_config()
        };
        assert!(matches_scope(&config, &task_in("/dl/somecat/Show.S01E01")));
        // Reported with the box's own casing (`00-common.md` rule 5).
        assert_eq!(
            matched_category(&config, "/dl/SomeCat/Show"),
            Some("SomeCat".to_string())
        );
        assert!(!matches_scope(&config, &task_in("/dl/somecategory/Show")));
        // Fixture `GetItems_when_category_is_set_should_ignore_downloads_in_wrong_folder`.
        assert!(!matches_scope(&config, &task_in("/some/path")));
    }

    #[test]
    fn with_both_set_the_destination_wins_in_the_scope_filter() {
        // The add path lands the task under the destination alone, so the
        // filter must not also demand the category segment.
        let config = FreeboxConfig {
            category: "somecat".to_string(),
            destination_directory: "/downloads".to_string(),
            ..test_config()
        };
        assert!(matches_scope(&config, &task_in("/downloads/Show.S01E01")));
        assert!(!matches_scope(&config, &task_in("/other/somecat/Show")));
        assert_eq!(add_settings_problem(&config), None);
        assert!(scope_exclusivity_problem(&config).is_some());
    }

    #[test]
    fn a_completed_task_is_always_a_directory() {
        let completed =
            torrent_to_completed(&test_config(), task_in("/downloads/Show.S01E01.1080p.WEB"));
        assert_eq!(
            completed.output_kind,
            Some(PluginDownloadOutputKind::Directory)
        );
        assert_eq!(completed.dest_dir, "/downloads/Show.S01E01.1080p.WEB");
    }

    #[test]
    fn the_reported_category_is_empty_when_none_is_configured() {
        let item = torrent_to_item(&test_config(), task_in("/dl/Show"));
        assert_eq!(item.category, None);
    }

    // -----------------------------------------------------------------------
    // Download directory (fixtures `Download_with_*_should_force_directory`)
    // -----------------------------------------------------------------------

    #[test]
    fn a_release_lands_in_its_own_folder_under_the_destination() {
        let config = FreeboxConfig {
            destination_directory: "/path/to/media/".to_string(),
            ..test_config()
        };
        assert_eq!(
            download_directory(&config, &add_request()).expect("a directory"),
            "/path/to/media/Droned.S01E01.Pilot.1080p.WEB-DL-DRONE"
        );
    }

    #[test]
    fn a_routed_directory_is_used_exactly_as_the_core_built_it() {
        let request = request_with(
            r#"{"release_title":"Droned.S01E01"}"#,
            r#"{"download_directory":"/path/isolated/"}"#,
            None,
        );
        assert_eq!(
            download_directory(&test_config(), &request).expect("a directory"),
            "/path/isolated"
        );
    }

    #[test]
    fn the_job_folder_falls_back_to_the_source_title_then_the_title() {
        let sourced: PluginDownloadClientAddRequest = serde_json::from_str(
            r#"{
                "source":{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn:btih:abc","source_title":"Droned.S01E02"},
                "release":{},
                "title":{"title_name":"Droned","media_facet":"series","tags":[]},
                "routing":{}
            }"#,
        )
        .expect("an add request");
        assert_eq!(job_folder_name(&sourced), Some("Droned.S01E02".to_string()));

        let titled: PluginDownloadClientAddRequest = serde_json::from_str(
            r#"{
                "source":{"kind":"magnet_uri","magnet_uri":"magnet:?xt=urn:btih:abc"},
                "release":{},
                "title":{"title_name":"Droned: The Movie","media_facet":"series","tags":[]},
                "routing":{}
            }"#,
        )
        .expect("an add request");
        assert_eq!(
            job_folder_name(&titled),
            Some("Droned - The Movie".to_string())
        );
    }

    #[test]
    fn clean_file_name_matches_sonarrs_default_naming_config() {
        assert_eq!(
            clean_file_name("Droned.S01E01.Pilot.1080p.WEB-DL-DRONE"),
            "Droned.S01E01.Pilot.1080p.WEB-DL-DRONE"
        );
        assert_eq!(clean_file_name("Show: Season 1"), "Show - Season 1");
        assert_eq!(clean_file_name("a/b\\c"), "a+b+c");
        assert_eq!(clean_file_name("What? *Now* <ok>"), "What! -Now- ok");
        assert_eq!(clean_file_name("  .Trimmed  "), "Trimmed");
    }

    #[test]
    fn a_torrent_upload_is_named_after_the_release() {
        assert_eq!(
            torrent_file_name(&add_request()),
            "Droned.S01E01.Pilot.1080p.WEB-DL-DRONE.torrent"
        );
    }

    // -----------------------------------------------------------------------
    // Post-add options
    // -----------------------------------------------------------------------

    #[test]
    fn nothing_is_sent_when_there_is_nothing_to_set() {
        assert!(torrent_settings_body(&test_config(), &add_request()).is_none());
    }

    #[test]
    fn add_paused_stops_the_task() {
        // Fixture `Download_should_pause_torrent_as_expected`.
        let config = FreeboxConfig {
            add_paused: true,
            ..test_config()
        };
        let body = torrent_settings_body(&config, &add_request()).expect("a body");
        assert_eq!(body["status"], "stopped");
    }

    #[test]
    fn an_explicit_initial_state_beats_the_configured_add_paused() {
        let config = FreeboxConfig {
            add_paused: true,
            ..test_config()
        };
        let started = request_with("{}", "{}", Some(r#"{"initial_state":"started"}"#));
        assert!(!request_paused(&config, &started));
        let paused = request_with("{}", "{}", Some(r#"{"initial_state":"paused"}"#));
        assert!(request_paused(&test_config(), &paused));
    }

    #[test]
    fn queue_first_follows_the_recency_of_the_release() {
        // Fixture `Download_should_queue_torrent_first_as_expected`.
        let cases = [
            (true, "first", "first", true),
            (true, "last", "first", true),
            (true, "first", "last", false),
            (true, "last", "last", false),
            (false, "first", "first", true),
            (false, "last", "first", false),
            (false, "first", "last", true),
            (false, "last", "last", false),
        ];
        for (recent, older, recent_priority, expected) in cases {
            let config = FreeboxConfig {
                older_priority: placement(Some(older)),
                recent_priority: placement(Some(recent_priority)),
                ..test_config()
            };
            let request = request_with(&format!(r#"{{"is_recent":{recent}}}"#), "{}", None);
            assert_eq!(
                queue_first(&config, &request),
                expected,
                "recent={recent} older={older} recent_priority={recent_priority}"
            );
        }
    }

    #[test]
    fn an_explicit_queue_placement_beats_the_configured_priority() {
        let config = FreeboxConfig {
            older_priority: PluginTorrentQueuePlacement::First,
            ..test_config()
        };
        let last = request_with("{}", "{}", Some(r#"{"queue_placement":"last"}"#));
        assert!(!queue_first(&config, &last));
        let first = request_with("{}", "{}", Some(r#"{"queue_placement":"first"}"#));
        assert!(queue_first(&test_config(), &first));
    }

    #[test]
    fn queue_pos_is_the_integer_the_api_documents() {
        // Sonarr sends the string "1" (`FreeboxDownloadProxy.cs:134-137`);
        // dev.freebox.fr/sdk/os/download/ documents `queue_pos int`.
        let config = FreeboxConfig {
            older_priority: PluginTorrentQueuePlacement::First,
            ..test_config()
        };
        let body = torrent_settings_body(&config, &add_request()).expect("a body");
        assert_eq!(body["queue_pos"], serde_json::json!(1));
    }

    #[test]
    fn the_seed_ratio_is_an_integer_percentage() {
        // Fixture `Download_should_define_seed_ratio_as_expected`: 1.5 -> 150.
        let request = request_with("{}", "{}", Some(r#"{"seed_goal_ratio":1.5}"#));
        assert_eq!(seed_ratio(&request), Some(150));
        let zero = request_with("{}", "{}", Some(r#"{"seed_goal_ratio":0.0}"#));
        assert_eq!(seed_ratio(&zero), Some(0));
        assert_eq!(seed_ratio(&add_request()), None);
        let body = torrent_settings_body(&test_config(), &request).expect("a body");
        assert_eq!(body["stop_ratio"], serde_json::json!(150));
    }

    #[test]
    fn a_release_seed_goal_is_used_when_the_torrent_options_carry_none() {
        let request = request_with(r#"{"seed_goal_ratio":2.0}"#, "{}", None);
        assert_eq!(seed_ratio(&request), Some(200));
    }

    // -----------------------------------------------------------------------
    // Settings validation (`FreeboxDownloadSettingsValidator`)
    // -----------------------------------------------------------------------

    #[test]
    fn a_valid_configuration_has_no_problem() {
        assert_eq!(settings_problem(&test_config()), None);
    }

    #[test]
    fn a_category_and_a_destination_cannot_be_combined() {
        let config = FreeboxConfig {
            category: "somecat".to_string(),
            destination_directory: "/path/to/media".to_string(),
            ..test_config()
        };
        assert_eq!(
            settings_problem(&config),
            Some("Cannot use 'Category' and 'Destination Directory' at the same time.".to_string())
        );
    }

    #[test]
    fn the_category_charset_matches_sonarrs_regex() {
        assert!(is_valid_category("somecat"));
        assert!(is_valid_category(".hidden-cat"));
        assert!(is_valid_category("SomeCat"));
        assert!(!is_valid_category("some cat"));
        assert!(!is_valid_category("cat1"));
        assert!(!is_valid_category("some/cat"));
    }

    #[test]
    fn the_api_url_must_be_a_path_not_an_absolute_url() {
        assert!(is_valid_url_base("/api/v1/"));
        assert!(!is_valid_url_base("http://mafreebox.freebox.fr/api/v1/"));
        assert!(!is_valid_url_base("/https://mafreebox.freebox.fr/api/"));
        let config = FreeboxConfig {
            api_url: "https://mafreebox.freebox.fr/api/v1/".to_string(),
            ..test_config()
        };
        assert!(settings_problem(&config).is_some());
    }

    #[test]
    fn missing_credentials_are_named_individually() {
        let no_id = FreeboxConfig {
            app_id: String::new(),
            ..test_config()
        };
        assert_eq!(
            settings_problem(&no_id),
            Some("'App ID' must not be empty.".to_string())
        );
        let no_token = FreeboxConfig {
            app_token: String::new(),
            ..test_config()
        };
        assert_eq!(
            settings_problem(&no_token),
            Some("'App Token' must not be empty.".to_string())
        );
        let bad_port = FreeboxConfig {
            port: "0".to_string(),
            ..test_config()
        };
        assert!(settings_problem(&bad_port).is_some());
    }

    // -----------------------------------------------------------------------
    // Error classification (`00-common.md` rule 4)
    // -----------------------------------------------------------------------

    fn failure(status: u16, body: &str) -> ApiError {
        interpret(&RawResponse {
            status,
            location: None,
            body: body.to_string(),
        })
        .expect_err("an error")
    }

    #[test]
    fn an_unreachable_api_is_upstream_unavailable() {
        assert_eq!(
            classify_transport_error("connection refused").code,
            PluginErrorCode::UpstreamUnavailable
        );
        assert_eq!(
            classify_transport_error("request timeout").code,
            PluginErrorCode::Temporary
        );
        assert_eq!(
            classify_transport_error("invalid peer certificate").code,
            PluginErrorCode::UpstreamUnavailable
        );
    }

    #[test]
    fn a_403_is_an_auth_failure_carrying_the_documented_description() {
        let error = failure(
            403,
            r#"{"success":false,"error_code":"invalid_token","msg":"Erreur"}"#,
        );
        assert_eq!(error.error.code, PluginErrorCode::AuthFailed);
        assert!(
            error
                .error
                .public_message
                .contains("invalid or has been revoked"),
            "{}",
            error.error.public_message
        );
    }

    #[test]
    fn an_expired_session_is_recognised_for_one_retry() {
        let error = failure(403, r#"{"success":false,"error_code":"auth_required"}"#);
        assert!(is_session_expired(&error));
        assert!(!is_session_expired(&failure(
            403,
            r#"{"success":false,"error_code":"invalid_token"}"#
        )));
    }

    #[test]
    fn a_404_points_at_the_api_url() {
        let error = failure(404, "");
        assert_eq!(error.error.code, PluginErrorCode::InvalidConfig);
        assert!(error.error.public_message.contains("API URL"));
    }

    #[test]
    fn a_redirect_is_a_configuration_problem() {
        let error = interpret(&RawResponse {
            status: 302,
            location: Some("https://mafreebox.freebox.fr/login.php".to_string()),
            body: String::new(),
        })
        .expect_err("an error");
        assert_eq!(error.error.code, PluginErrorCode::InvalidConfig);
        assert!(error.error.public_message.contains("login.php"));
    }

    #[test]
    fn the_download_api_error_table_maps_to_typed_codes() {
        let cases = [
            ("task_not_found", PluginErrorCode::Permanent),
            ("invalid_file", PluginErrorCode::Permanent),
            ("invalid_url", PluginErrorCode::Permanent),
            ("exists", PluginErrorCode::Permanent),
            ("too_many_tasks", PluginErrorCode::Temporary),
            ("hibernating", PluginErrorCode::Temporary),
            ("out_of_memory", PluginErrorCode::Temporary),
            ("ctx_file_error", PluginErrorCode::Temporary),
            ("invalid_api_version", PluginErrorCode::InvalidConfig),
            ("port_outside_range", PluginErrorCode::InvalidConfig),
            ("ratelimited", PluginErrorCode::RateLimited),
            ("insufficient_rights", PluginErrorCode::AuthFailed),
            ("not_implemented", PluginErrorCode::Unsupported),
        ];
        for (code, expected) in cases {
            let error = failure(
                200,
                &format!(r#"{{"success":false,"error_code":"{code}"}}"#),
            );
            assert_eq!(error.error.code, expected, "{code}");
            assert!(error.is(code), "{code}");
        }
    }

    #[test]
    fn a_5xx_is_temporary_and_an_unknown_4xx_is_permanent() {
        assert_eq!(failure(503, "").error.code, PluginErrorCode::Temporary);
        assert_eq!(failure(418, "").error.code, PluginErrorCode::Permanent);
    }

    #[test]
    fn a_body_that_is_not_a_freebox_response_names_the_api_url() {
        let error = failure(200, "<html>login</html>");
        assert_eq!(error.error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn a_successful_delete_needs_no_result() {
        // `DELETE /downloads/{id}` answers `{"success": true}`.
        let result = interpret(&RawResponse {
            status: 200,
            location: None,
            body: r#"{"success":true}"#.to_string(),
        })
        .expect("success");
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // Permissions and version discovery
    // -----------------------------------------------------------------------

    #[test]
    fn a_session_without_the_downloader_permission_is_refused() {
        let granted: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"downloader":true,"settings":true}"#).expect("permissions");
        assert_eq!(permission_problem(Some(&granted)), None);

        let denied: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"settings":true}"#).expect("permissions");
        assert!(permission_problem(Some(&denied)).is_some());

        // Firmware that does not report permissions at all is not refused.
        assert_eq!(permission_problem(None), None);
    }

    #[test]
    fn the_api_version_document_is_parsed_from_the_http_root() {
        let discovery: FreeboxApiVersion = serde_json::from_str(
            r#"{"uid":"23b8","device_name":"Freebox Server","api_version":"8.0","api_base_url":"/api/","device_type":"FreeboxServer1,2","https_port":3615}"#,
        )
        .expect("api_version");
        assert_eq!(discovery.api_version, "8.0");
        assert_eq!(discovery.api_base_url, "/api/");
    }

    #[test]
    fn a_configured_api_version_above_the_box_is_reported() {
        let discovery = FreeboxApiVersion {
            api_version: "8.0".to_string(),
            api_base_url: "/api/".to_string(),
        };
        assert_eq!(configured_api_major("/api/v1/"), Some(1));
        assert_eq!(api_version_problem(&test_config(), &discovery), None);

        let ahead = FreeboxConfig {
            api_url: "/api/v9/".to_string(),
            ..test_config()
        };
        let problem = api_version_problem(&ahead, &discovery).expect("a problem");
        assert!(problem.contains("v9"), "{problem}");
        assert!(problem.contains("/api/v8/"), "{problem}");
    }

    #[test]
    fn localhost_is_recognised_from_the_host_not_the_url() {
        assert!(is_localhost_host("127.0.0.1"));
        assert!(is_localhost_host("localhost"));
        assert!(is_localhost_host("::1"));
        assert!(!is_localhost_host("mafreebox.freebox.fr"));
    }

    // -----------------------------------------------------------------------
    // Descriptor and post-import contract
    // -----------------------------------------------------------------------

    #[test]
    fn the_descriptor_declares_no_post_import_mark() {
        // Freebox has no tag, label, category or view to write back to, so both
        // marks are declared absent and the core skips the handoff entirely
        // (`00-common.md` rule 3).
        let value = descriptor();
        assert_eq!(value["provider"]["capabilities"]["mark_imported"], false);
        assert_eq!(
            value["provider"]["capabilities"]["mark_imported_non_destructive"],
            false
        );
        assert!(functions().mark_imported_non_destructive.is_none());
    }

    #[test]
    fn mark_imported_is_an_acknowledged_no_op() {
        let request = r#"{"client_item_id":"42"}"#;
        let raw = scryer_download_mark_imported(request.to_string()).expect("mark imported");
        let result: PluginResult<()> = serde_json::from_str(&raw).expect("a plugin result");
        assert!(matches!(result, PluginResult::Ok(())));
    }

    #[test]
    fn the_descriptor_only_claims_directory_isolation() {
        let value = descriptor();
        assert_eq!(
            value["provider"]["isolation_modes"],
            serde_json::json!(["directory"])
        );
        assert_eq!(
            value["provider"]["capabilities"]["torrent"]["isolation_modes"],
            serde_json::json!(["directory"])
        );
    }

    #[test]
    fn the_descriptor_claims_the_pause_resume_freebox_actually_has() {
        let capabilities = &descriptor()["provider"]["capabilities"];
        assert_eq!(capabilities["pause"], true);
        assert_eq!(capabilities["resume"], true);
        assert_eq!(capabilities["force_start"], false);
        assert_eq!(capabilities["torrent"]["supports_seed_time_limit"], false);
    }

    #[test]
    fn failed_history_is_empty_because_freebox_keeps_none() {
        let raw = scryer_download_list_history(String::new()).expect("list history");
        let result: PluginResult<Vec<PluginDownloadItem>> =
            serde_json::from_str(&raw).expect("a plugin result");
        match result {
            PluginResult::Ok(items) => assert!(items.is_empty()),
            PluginResult::Err(error) => panic!("unexpected error: {error:?}"),
        }
    }

    #[test]
    fn a_force_start_is_refused_as_unsupported() {
        let raw = scryer_download_control(
            r#"{"client_item_id":"42","action":"force_start","remove_data":false}"#.to_string(),
        )
        .expect("control");
        let result: PluginResult<()> = serde_json::from_str(&raw).expect("a plugin result");
        match result {
            PluginResult::Err(error) => assert_eq!(error.code, PluginErrorCode::Unsupported),
            PluginResult::Ok(()) => panic!("force start must not be accepted"),
        }
    }

    // -----------------------------------------------------------------------
    // Encoding
    // -----------------------------------------------------------------------

    #[test]
    fn the_add_form_is_url_encoded_and_the_directory_base64() {
        let form = [
            (
                "download_url".to_string(),
                "magnet:?xt=urn:btih:abc&dn=a b".to_string(),
            ),
            ("download_dir".to_string(), STANDARD.encode("/some/path")),
        ];
        let encoded = encode_form(&form);
        assert!(encoded.starts_with("download_url=magnet%3A%3Fxt%3Durn%3Abtih%3Aabc%26dn%3Da%20b"));
        assert!(encoded.ends_with("download_dir=L3NvbWUvcGF0aA%3D%3D"));
    }

    #[test]
    fn the_upload_body_carries_the_fields_before_the_file() {
        let fields = [("download_dir".to_string(), "L3NvbWUvcGF0aA==".to_string())];
        let body = encode_multipart("b", &fields, "release.torrent", b"d8:announce");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"download_dir\"\r\n\r\nL3NvbWUvcGF0aA==\r\n"));
        assert!(text.contains("name=\"download_file\"; filename=\"release.torrent\""));
        assert!(text.contains("Content-Type: application/x-bittorrent"));
        assert!(text.ends_with("\r\n--b--\r\n"));
    }

    #[test]
    fn the_user_agent_carries_the_crate_version() {
        assert_eq!(
            USER_AGENT,
            format!("scryer-freebox-plugin/{}", env!("CARGO_PKG_VERSION"))
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
        mark_imported_non_destructive: None,
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
