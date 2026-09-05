//! Tribler download client.
//!
//! Reconciled against Sonarr's `TriblerDownloadClient` (the behavioural floor)
//! and, where the two disagree, against Tribler's own REST API source, which
//! wins. The routes and payloads used here are
//! `src/tribler/core/libtorrent/restapi/downloads_endpoint.py` and
//! `src/tribler/core/restapi/{rest_manager,settings_endpoint}.py` on Tribler
//! `v8.4.3` (current stable, 2026-06-18), cross-checked against `v8.0.7` — the
//! version Sonarr's provider message names — and `v7.14.0` for the fields that
//! were renamed on the way to 8.x.
//!
//! Scryer's contract shapes the rest: the core owns removal, seeding policy,
//! path mapping and post-import handoff; the plugin's job is to observe Tribler
//! honestly (tri-state `can_remove`, richer `DownloadItemState`s, `completed_at`,
//! rates, content paths) and to execute what the core routes to it.

use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, DownloadClientCapabilities,
    DownloadClientDescriptor, DownloadControlAction, DownloadInputKind, DownloadIsolationMode,
    DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, PluginTorrentItem, ProviderDescriptor, SDK_VERSION,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

macro_rules! warn_log {
    ($($argument:tt)*) => {
        scryer_plugin_pdk::log::log(
            scryer_plugin_pdk::log::LogLevel::Warn,
            &format!($($argument)*),
        )
    };
}

macro_rules! debug_log {
    ($($argument:tt)*) => {
        scryer_plugin_pdk::log::log(
            scryer_plugin_pdk::log::LogLevel::Debug,
            &format!($($argument)*),
        )
    };
}

/// The Tribler release Sonarr's `DownloadClientTriblerProviderMessage` names
/// (`TriblerDownloadClient.cs:43`), and the floor this plugin targets.
const MINIMUM_TESTED_VERSION: &str = "8.0.7";
/// The Tribler release this plugin's field and status tables were verified
/// against.
const NEWEST_TESTED_VERSION: &str = "8.4.3";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct TriblerConfig {
    api_root: String,
    api_key: String,
    url_base: String,
    category: String,
    directory: String,
    /// `None` when the stored value is not an integer at all; validation turns
    /// that into an `InvalidConfig` naming the field, and everything else falls
    /// back to Tribler's own default of one hop.
    anonymity_level: Option<i64>,
    anonymity_level_raw: String,
    safe_seeding: bool,
}

impl TriblerConfig {
    fn from_config() -> Self {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "20100".to_string());
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
        let anonymity_level_raw = config_value("anonymity_level").unwrap_or_default();
        Self {
            api_root: format!("{}/api", base.trim_end_matches('/')),
            api_key: config_value("api_key").unwrap_or_default(),
            url_base,
            category: config_value("category").unwrap_or_default(),
            directory: config_value("directory").unwrap_or_default(),
            anonymity_level: if anonymity_level_raw.is_empty() {
                Some(1)
            } else {
                anonymity_level_raw.parse().ok()
            },
            anonymity_level_raw,
            safe_seeding: config_bool("safe_seeding", true),
        }
    }

    fn anonymity_hops(&self) -> i64 {
        self.anonymity_level.unwrap_or(1)
    }
}

/// `TriblerSettingsValidator` (`TriblerDownloadSettings.cs:10-24`), plus the one
/// rule Sonarr does not have: Tribler itself rejects an anonymous download that
/// is not safe-seeding with HTTP 400 "Cannot set anonymous download without safe
/// seeding enabled" (`downloads_endpoint.py::create_dconfig_from_params`), so a
/// configuration that would always 400 is a configuration error, not a runtime
/// one.
///
/// Sonarr reports these as per-field `ValidationFailure`s; Scryer's equivalent
/// is an `InvalidConfig` whose public message names the field.
fn validate_config(config: &TriblerConfig) -> Result<(), PluginError> {
    validate_add_config(config)?;
    // Reported by the connection test only: `download_directory`,
    // `output_roots` and `item_category` all let the directory win, so the
    // combination cannot misroute a grab, and refusing adds for a
    // configuration that worked before would fail grabs on upgrade.
    if !config.category.is_empty() && !config.directory.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "Cannot use Category and Directory: clear one of them.",
        ));
    }
    Ok(())
}

/// The rules an add must satisfy before it commits anything.
fn validate_add_config(config: &TriblerConfig) -> Result<(), PluginError> {
    if config.api_key.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "API Key is required: Tribler's REST API key is the `[api] key` value from triblerd.conf.",
        ));
    }
    if !is_valid_url_base(&config.url_base) {
        return Err(detailed_error(
            PluginErrorCode::InvalidConfig,
            "URL Base must be a URL path (for example `/tribler`), not a full URL.",
            config.url_base.clone(),
        ));
    }
    if !is_valid_category(&config.category) {
        return Err(detailed_error(
            PluginErrorCode::InvalidConfig,
            "Category allows only the characters a-z and -, with an optional leading dot.",
            config.category.clone(),
        ));
    }
    match config.anonymity_level {
        None => {
            return Err(detailed_error(
                PluginErrorCode::InvalidConfig,
                "Anonymity Level must be a whole number of hops.",
                config.anonymity_level_raw.clone(),
            ));
        }
        Some(level) if level < 0 => {
            return Err(detailed_error(
                PluginErrorCode::InvalidConfig,
                "Anonymity Level must be zero or greater; zero disables anonymous downloading.",
                config.anonymity_level_raw.clone(),
            ));
        }
        Some(level) if level > 0 && !config.safe_seeding => {
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                "Tribler refuses an anonymous download with safe seeding off: enable Safe Seeding or set Anonymity Level to 0.",
            ));
        }
        Some(_) => {}
    }
    Ok(())
}

/// Sonarr's `ValidUrlBase` (`RuleBuilderExtensions.cs:55-58`) is the negative
/// lookahead `^(?!\/?https?://[-_a-z0-9.]+)`: a URL base may not be a URL.
fn is_valid_url_base(url_base: &str) -> bool {
    let trimmed = url_base.trim().trim_start_matches('/').to_ascii_lowercase();
    !(trimmed.starts_with("http://") || trimmed.starts_with("https://"))
}

/// Sonarr's `^\.?[-a-z]*$` with `RegexOptions.IgnoreCase`
/// (`TriblerDownloadSettings.cs:18`), spelled without a regex dependency.
fn is_valid_category(category: &str) -> bool {
    let body = category.strip_prefix('.').unwrap_or(category);
    body.chars().all(|ch| ch == '-' || ch.is_ascii_alphabetic())
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct DownloadsResponse {
    #[serde(default)]
    downloads: Vec<TriblerDownload>,
}

/// One entry of `GET /downloads`.
///
/// Field names are the 8.x ones (`downloads_endpoint.py::get_downloads`), with
/// the 7.x spellings kept as fallbacks where they were renamed:
/// `all_time_ratio`/`all_time_upload`/`all_time_download` were `ratio`/
/// `total_up`/`total_down` up to and including `v7.14.0`. Sonarr's model has
/// `TotalDown` for the same reason (`TriblerDownloadClientApi.cs:79`).
#[derive(Default, Deserialize, Clone)]
struct TriblerDownload {
    #[serde(default)]
    name: String,
    #[serde(default)]
    progress: Option<f64>,
    #[serde(default)]
    infohash: String,
    #[serde(default)]
    eta: Option<f64>,
    #[serde(default)]
    all_time_upload: Option<i64>,
    #[serde(default)]
    all_time_download: Option<i64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    all_time_ratio: Option<f64>,
    /// 7.x name for `all_time_ratio`.
    #[serde(default)]
    ratio: Option<f64>,
    /// Parsed but deliberately unused. Sonarr's only use of it is the seeding
    /// time goal (`TriblerDownloadClient.cs:205-210`), where it is the wrong
    /// clock — see `derive_can_remove` — and Scryer's queue item has no
    /// "added at" of its own to carry it in.
    #[allow(dead_code)]
    #[serde(default)]
    time_added: Option<i64>,
    /// `tdef.atp.completed_time` — the unix second at which the payload
    /// finished, and therefore the second seeding started. Tribler `>= 8.3.0`
    /// only; `0` means "not finished".
    #[serde(default)]
    time_finished: Option<i64>,
    /// The effective seeding-ratio goal for *this* download: the per-download
    /// override, or the global default when there is none
    /// (`download.py::get_seeding_ratio`). Tribler `>= 8.4.1` only.
    #[serde(default)]
    seeding_ratio: Option<f64>,
    #[serde(default)]
    error: Option<String>,
    /// 7.x name for `all_time_download`.
    #[serde(default)]
    total_down: Option<i64>,
    /// 7.x name for `all_time_upload`.
    #[serde(default)]
    total_up: Option<i64>,
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    destination: String,
    #[serde(default)]
    speed_down: Option<f64>,
    #[serde(default)]
    speed_up: Option<f64>,
}

impl TriblerDownload {
    fn status(&self) -> &str {
        self.status.as_deref().unwrap_or_default()
    }

    fn progress(&self) -> f64 {
        self.progress.unwrap_or_default().clamp(0.0, 1.0)
    }

    fn seed_ratio(&self) -> Option<f64> {
        self.all_time_ratio.or(self.ratio)
    }

    fn uploaded_bytes(&self) -> Option<i64> {
        self.all_time_upload.or(self.total_up)
    }

    fn downloaded_bytes(&self) -> Option<i64> {
        self.all_time_download.or(self.total_down)
    }

    /// Tribler serialises "no error" as the empty string, not `null`
    /// (`downloads_endpoint.py`: `repr(state.get_error()) if ... else ""`), so
    /// emptiness is the test — the same one Sonarr makes
    /// (`TriblerDownloadClient.cs:144`).
    fn error_message(&self) -> Option<&str> {
        self.error.as_deref().filter(|value| !value.is_empty())
    }

    /// The second seeding started, from `time_finished`. `None` on Tribler
    /// before 8.3.0, which does not report it.
    fn seeding_started_at(&self) -> Option<i64> {
        self.time_finished.filter(|value| *value > 0)
    }
}

#[derive(Default, Deserialize)]
struct FilesResponse {
    #[serde(default)]
    files: Vec<TriblerFile>,
}

#[derive(Default, Deserialize, Clone)]
struct TriblerFile {
    #[serde(default)]
    name: String,
}

#[derive(Default, Deserialize)]
struct AddDownloadResponse {
    #[serde(default)]
    infohash: String,
}

#[derive(Default, Deserialize)]
struct VersionResponse {
    #[serde(default)]
    version: String,
}

#[derive(Default, Deserialize)]
struct TriblerSettingsResponse {
    #[serde(default)]
    settings: TriblerSettings,
}

/// Tribler moved the libtorrent download defaults under a `libtorrent` key in
/// 8.x (`tribler_config.py`); in 7.x `download_defaults` sat at the top level
/// (`v7.14.0 tribler_config.py:53-54`). Both are parsed so a 7.x instance still
/// yields a save-as root instead of an empty one.
#[derive(Default, Deserialize)]
struct TriblerSettings {
    #[serde(default)]
    libtorrent: LibTorrent,
    #[serde(default)]
    download_defaults: DownloadDefaults,
}

impl TriblerSettings {
    fn download_defaults(&self) -> &DownloadDefaults {
        if self.libtorrent.download_defaults.save_as.is_empty()
            && !self.download_defaults.save_as.is_empty()
        {
            &self.download_defaults
        } else {
            &self.libtorrent.download_defaults
        }
    }

    fn save_as(&self) -> &str {
        self.download_defaults().save_as.trim_end_matches('/')
    }
}

#[derive(Default, Deserialize)]
struct LibTorrent {
    #[serde(default)]
    download_defaults: DownloadDefaults,
}

#[derive(Default, Deserialize, Clone)]
struct DownloadDefaults {
    #[serde(default, rename = "saveas")]
    save_as: String,
    #[serde(default)]
    seeding_mode: Option<String>,
    #[serde(default)]
    seeding_ratio: Option<f64>,
    #[serde(default)]
    seeding_time: Option<f64>,
}

#[derive(Serialize)]
struct AddDownloadRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<String>,
    uri: String,
    safe_seeding: bool,
    anon_hops: i64,
}

#[derive(Serialize)]
struct RemoveDownloadRequest {
    remove_data: bool,
}

#[derive(Serialize)]
struct UpdateDownloadRequest {
    state: &'static str,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

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

fn respond<T: Serialize>(result: Result<T, PluginError>) -> FnResult<String> {
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
            "Scryer sent a request this Tribler plugin could not read.",
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

/// Every Tribler REST failure carries the same envelope,
/// `{"error": {"handled": bool, "message": str}}`
/// (`rest_manager.py::error_middleware`, `ApiKeyMiddleware`), so the message is
/// worth lifting out of the body for `debug_message`.
fn tribler_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

fn error_detail(body: &str) -> String {
    tribler_error_message(body).unwrap_or_else(|| truncate(body))
}

/// The typed equivalents of Sonarr's two Tribler exception paths
/// (`TriblerDownloadClientProxy.cs:62-70`): 401 becomes a
/// `DownloadClientAuthenticationException`, which `TestConnection` reports as an
/// `ApiKey` failure, and every other HTTP failure becomes a
/// `DownloadClientUnavailableException` reported against `Host`
/// (`TriblerDownloadClient.cs:275-289`).
///
/// Scryer's contract wants more than those two: the host runs plugin HTTP with
/// redirects disabled, so a reverse proxy bouncing the API to a login page
/// arrives here as a 3xx, and a 404 on the API root means the URL base is wrong
/// rather than that Tribler is down. Both are configuration faults, not
/// transient ones.
fn classify_http_status(response: &TriblerHttpResponse) -> Option<PluginError> {
    let status = response.status;
    let body = response.body.as_str();
    match status {
        200..=299 => None,
        300..=399 => Some(detailed_error(
            PluginErrorCode::InvalidConfig,
            match response.location.as_deref().map(str::trim) {
                Some(location) if !location.is_empty() => format!(
                    "Tribler's REST API redirected to {location}; check host, port and URL base."
                ),
                _ => "Tribler's REST API redirected the request; check host, port and URL base."
                    .to_string(),
            },
            error_detail(body),
        )),
        401 | 403 => Some(detailed_error(
            PluginErrorCode::AuthFailed,
            "Tribler rejected the API key.",
            error_detail(body),
        )),
        404 => Some(detailed_error(
            PluginErrorCode::InvalidConfig,
            "No Tribler REST API was found at this address; check host, port and URL base.",
            error_detail(body),
        )),
        400 => Some(detailed_error(
            PluginErrorCode::Permanent,
            "Tribler rejected the request as invalid.",
            error_detail(body),
        )),
        500..=599 => Some(detailed_error(
            PluginErrorCode::Temporary,
            format!("Tribler returned HTTP {status}."),
            error_detail(body),
        )),
        _ => Some(detailed_error(
            PluginErrorCode::Permanent,
            format!("Tribler returned HTTP {status}."),
            error_detail(body),
        )),
    }
}

/// The host hands transport failures back as a string, so classification is by
/// substring. This is the closest this surface gets to the exception Sonarr
/// turns into its `Host` "Unable to connect" validation failure.
fn classify_transport_error(detail: &str) -> PluginError {
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("timeout") || lowered.contains("timed out") {
        detailed_error(
            PluginErrorCode::Temporary,
            "Tribler did not answer in time.",
            detail,
        )
    } else if lowered.contains("certificate")
        || lowered.contains("tls")
        || lowered.contains("ssl")
        || lowered.contains("trust")
    {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Tribler: certificate validation failed.",
            detail,
        )
    } else {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Tribler, please check your settings.",
            detail,
        )
    }
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "tribler".to_string(),
        name: "Tribler".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "tribler".to_string(),
            provider_aliases: vec![],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            // Sonarr throws `NotSupportedException` from `AddFromTorrentFile`
            // (`TriblerDownloadClient.cs:234-238`) with a TODO saying 8.x can do
            // it. It can: `PUT /downloads` with `Content-Type:
            // applications/x-bittorrent` bdecodes the raw body as the torrent
            // and reads its options from the query string
            // (`downloads_endpoint.py::add_download`, unchanged from `v8.0.7`
            // through `v8.4.3`). Magnets stay preferred, matching Sonarr's
            // `PreferTorrentFile => false`.
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentBytes,
            ],
            isolation_modes: vec![DownloadIsolationMode::Directory],
            capabilities: DownloadClientCapabilities {
                // `PATCH /downloads/{infohash}` with `{"state": "stop"}` /
                // `{"state": "resume"}` — present in every Tribler this plugin
                // supports (`downloads_endpoint.py::update_download`, `v7.14.0`
                // through `v8.4.3`). Sonarr never wires these up because its
                // download-client contract has no pause/resume.
                pause: true,
                resume: true,
                remove: true,
                remove_with_data: true,
                mark_imported: false,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                seed_limits: false,
                start_paused: false,
                force_start: false,
                per_download_directory: true,
                host_fs_required: false,
                test_connection: true,
                torrent: Some(DownloadTorrentCapabilities {
                    supported_sources: vec![
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentBytes,
                    ],
                    preferred_sources: vec![DownloadInputKind::MagnetUri],
                    isolation_modes: vec![DownloadIsolationMode::Directory],
                    supports_seed_ratio_limit: false,
                    supports_seed_time_limit: false,
                    supports_start_paused: false,
                    supports_force_start: false,
                    supports_sequential_download: false,
                    supports_first_last_piece_priority: false,
                    supports_content_layout: false,
                    supports_skip_checking: false,
                    supports_auto_management: false,
                    supports_post_import_isolation: false,
                    reports_content_paths: true,
                    supports_anonymity_hops: true,
                    supports_safe_seeding: true,
                    ..DownloadTorrentCapabilities::default()
                }),
                // SDK 3.10 addition. `false` is the SDK's own default and therefore exactly
                // what this client's pre-3.10 descriptor already meant to a 3.10 host;
                // advertising category-scoped feedback would be a behaviour change, not a
                // transport one, so it stays off across the component migration.
                category_scoped_feedback: false,
                // Tribler has no label, tag, category or view — a "category" here
                // is only a child directory under its save-as root — so there is
                // nothing to write back after an import. See
                // `scryer_download_mark_imported`.
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
            Some("localhost"),
            None,
        ),
        field(
            "port",
            "Port",
            ConfigFieldType::Number,
            true,
            Some("20100"),
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
            None,
            Some("Path Tribler's REST API is served under, e.g. /tribler"),
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("The [api] key value from triblerd.conf"),
        ),
        field(
            "category",
            "Category",
            ConfigFieldType::String,
            false,
            None,
            Some("Child directory under Tribler's save location; a-z and - only"),
        ),
        field(
            "directory",
            "Directory",
            ConfigFieldType::Path,
            false,
            None,
            Some("Optional location to put downloads in; cannot be combined with Category"),
        ),
        field(
            "anonymity_level",
            "Anonymity Level",
            ConfigFieldType::Number,
            false,
            Some("1"),
            Some("Number of proxies used when downloading; 0 disables anonymous downloading"),
        ),
        field(
            "safe_seeding",
            "Safe Seeding",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            Some("When enabled, only seed through proxies"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

pub fn scryer_download_add(input: String) -> FnResult<String> {
    respond(add(&input))
}

fn add(input: &str) -> Result<PluginDownloadClientAddResponse, PluginError> {
    let request: PluginDownloadClientAddRequest = parse_request(input)?;
    let config = TriblerConfig::from_config();
    validate_add_config(&config)?;

    let destination = download_directory(&config, &request)?;
    let safe_seeding = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.safe_seeding)
        .unwrap_or(config.safe_seeding);
    let anon_hops = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.anonymity_hops)
        .map(i64::from)
        .unwrap_or_else(|| config.anonymity_hops());
    // Tribler applies the same rule to a per-download override that it applies
    // to the configured defaults, and answers HTTP 400
    // (`downloads_endpoint.py::create_dconfig_from_params`). Refuse before the
    // round trip so the message names the cause.
    if anon_hops > 0 && !safe_seeding {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "Tribler refuses an anonymous download with safe seeding off: enable Safe Seeding or set Anonymity Level to 0.",
        ));
    }

    // Preference order matters. A magnet is what Sonarr sends and what Tribler
    // handles best. Torrent bytes come next: the core has already fetched them
    // with the indexer's own credentials, which Tribler does not have. A bare
    // URL is the last resort — `start_download_from_uri` accepts a magnet, an
    // `http(s)` URL or a `file:` path, but Tribler fetching an indexer URL
    // itself sends no indexer cookies.
    let response: AddDownloadResponse = if let Some(uri) = request.source.magnet_uri.clone() {
        add_uri(&config, uri, destination, safe_seeding, anon_hops)?
    } else if let Some(encoded) = request.source.torrent_bytes_base64.as_deref() {
        let bytes = STANDARD.decode(encoded).map_err(|error| {
            detailed_error(
                PluginErrorCode::Permanent,
                "Scryer sent a torrent file this Tribler plugin could not decode.",
                error.to_string(),
            )
        })?;
        add_torrent_bytes(&config, bytes, destination, safe_seeding, anon_hops)?
    } else if let Some(uri) = request
        .source
        .torrent_url
        .clone()
        .or_else(|| request.source.download_url.clone())
    {
        add_uri(&config, uri, destination, safe_seeding, anon_hops)?
    } else {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "Tribler needs a magnet link, a torrent URL or torrent file contents to start a download.",
        ));
    };

    let hash = normalize_hash(&response.infohash);
    if hash.is_empty() {
        return Err(detailed_error(
            PluginErrorCode::Temporary,
            "Tribler accepted the download but did not report an info hash.",
            truncate(&response.infohash),
        ));
    }
    Ok(PluginDownloadClientAddResponse {
        client_item_id: hash.clone(),
        info_hash: Some(hash),
    })
}

/// `PUT /downloads` with a JSON body naming the URI, exactly as Sonarr's
/// `AddFromMagnetLink` does (`TriblerDownloadClientProxy.cs:108-117`).
fn add_uri(
    config: &TriblerConfig,
    uri: String,
    destination: Option<String>,
    safe_seeding: bool,
    anon_hops: i64,
) -> Result<AddDownloadResponse, PluginError> {
    let body = serde_json::to_value(AddDownloadRequest {
        destination,
        uri,
        safe_seeding,
        anon_hops,
    })
    .map_err(encoding_error)?;
    request_json(config, "PUT", "/downloads", Some(body))
}

/// `PUT /downloads` with the raw torrent as the body.
///
/// Tribler switches on the request's content type and, on that branch, reads
/// every option from the *query string* rather than the body
/// (`downloads_endpoint.py::add_download`), so the destination and the anonymity
/// options have to be percent-encoded into the URL. The content type it matches
/// is the literal `applications/x-bittorrent` — the misspelling is Tribler's and
/// has been stable from `v8.0.7` through `v8.4.3`.
fn add_torrent_bytes(
    config: &TriblerConfig,
    bytes: Vec<u8>,
    destination: Option<String>,
    safe_seeding: bool,
    anon_hops: i64,
) -> Result<AddDownloadResponse, PluginError> {
    let mut params = vec![
        ("safe_seeding".to_string(), safe_seeding.to_string()),
        ("anon_hops".to_string(), anon_hops.to_string()),
    ];
    if let Some(destination) = destination {
        params.push(("destination".to_string(), destination));
    }
    let path = format!("/downloads?{}", encode_query(&params));
    let response = send(
        config,
        "PUT",
        &path,
        Some(bytes),
        "applications/x-bittorrent",
    )?;
    if let Some(error) = classify_http_status(&response) {
        return Err(error);
    }
    parse_json(&response.body)
}

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    respond(list_queue())
}

fn list_queue() -> Result<Vec<PluginDownloadItem>, PluginError> {
    let config = TriblerConfig::from_config();
    let settings = get_settings(&config)?;
    let now = current_unix_seconds();
    get_downloads(&config)?
        .into_iter()
        .filter(is_visible_download)
        .map(|download| torrent_to_item(&config, &settings, download, now))
        .collect()
}

/// Scryer merges failed history into the queue itself; Sonarr has no separate
/// history call for Tribler either (`GetItems` is the only listing).
/// Re-listing `/downloads` here would double every poll's request count — and,
/// before the per-hash file cache below, every download's files with it — for a
/// list the bridge already has.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    respond(Ok::<Vec<PluginDownloadItem>, PluginError>(Vec::new()))
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    respond(list_completed())
}

fn list_completed() -> Result<Vec<PluginCompletedDownload>, PluginError> {
    let config = TriblerConfig::from_config();
    let settings = get_settings(&config)?;
    get_downloads(&config)?
        .into_iter()
        .filter(is_visible_download)
        // Completed downloads are those whose data is fully present; waiting for the seeding
        // goal here would keep finished payloads out of import indefinitely.
        .filter(|download| {
            matches!(download.status(), "SEEDING" | "STOPPED") && is_data_complete(download)
        })
        .map(|download| torrent_to_completed(&config, &settings, download))
        .collect()
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    respond(control(&input))
}

fn control(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientControlRequest = parse_request(input)?;
    let config = TriblerConfig::from_config();
    let hash = normalize_hash(&request.client_item_id);
    match request.action {
        DownloadControlAction::Remove => {
            let body = serde_json::to_value(RemoveDownloadRequest {
                remove_data: request.remove_data,
            })
            .map_err(encoding_error)?;
            request_ignoring_missing(&config, "DELETE", &format!("/downloads/{hash}"), Some(body))?;
            forget_download_state(&hash);
        }
        DownloadControlAction::Pause | DownloadControlAction::Resume => {
            let state = if request.action == DownloadControlAction::Pause {
                "stop"
            } else {
                "resume"
            };
            let body =
                serde_json::to_value(UpdateDownloadRequest { state }).map_err(encoding_error)?;
            request_ignoring_missing(&config, "PATCH", &format!("/downloads/{hash}"), Some(body))?;
        }
        DownloadControlAction::ForceStart => {
            return Err(plugin_error(
                PluginErrorCode::Unsupported,
                "Tribler has no force-start control.",
            ));
        }
    }
    Ok(())
}

/// Tribler has no label, tag, category or view — its "category" is a child
/// directory chosen at add time and never mutated afterwards — so there is
/// nothing to write back to it after an import.
///
/// The descriptor says so (`mark_imported_non_destructive: false`), which is what
/// the core reads before it schedules a handoff, and the function table leaves
/// the non-destructive slot empty so the bridge answers `Ok(())` itself. This
/// body exists only because the legacy table requires the destructive slot to be
/// filled; the core has no caller for it. Removing a finished torrent stays the
/// core's decision through the seeding gate, never the plugin's.
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
    let config = TriblerConfig::from_config();
    let settings = get_settings(&config)?;
    let version = server_version(&config);
    Ok(PluginDownloadClientStatus {
        is_localhost: Some(is_localhost_url(&config.api_root)),
        remote_output_roots: output_roots(&config, &settings),
        removes_completed_downloads: Some(false),
        sorting_mode: Some("tribler-api".to_string()),
        warnings: provider_warnings(version.as_deref()),
        version,
    })
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    respond(test_connection())
}

/// Sonarr's `Test` is `TestConnection()`, which is `GetItems()` plus the two
/// exception-to-field mappings (`TriblerDownloadClient.cs:240-296`); the settings
/// validator runs separately, before the provider is saved
/// (`TriblerDownloadSettings.cs:69-72`). Scryer has one entry point for both, so
/// the validator runs here first and the listing proves the credentials.
fn test_connection() -> Result<String, PluginError> {
    let config = TriblerConfig::from_config();
    validate_config(&config)?;
    let _ = get_settings(&config)?;
    let _ = get_downloads(&config)?;
    Ok("ok".to_string())
}

// ---------------------------------------------------------------------------
// Paths and roots
// ---------------------------------------------------------------------------

/// Where an add actually lands, in the precedence Sonarr uses
/// (`GetDownloadDirectory`, `TriblerDownloadClient.cs:250-266`) with Scryer's
/// routed per-download directory in front of it.
fn download_directory(
    config: &TriblerConfig,
    request: &PluginDownloadClientAddRequest,
) -> Result<Option<String>, PluginError> {
    if let Some(directory) = request
        .routing
        .download_directory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(directory.to_string()));
    }
    if !config.directory.is_empty() {
        return Ok(Some(config.directory.clone()));
    }
    if config.category.is_empty() {
        return Ok(None);
    }
    let settings = get_settings(config)?;
    Ok(category_root(config, &settings))
}

/// The directory an add with a configured category writes to.
///
/// Sonarr disagrees with itself here: `GetDownloadDirectory` adds to
/// `{saveas}/{category}` (`TriblerDownloadClient.cs:265`) while `GetStatus`
/// reports the root as `{saveas}/.{category}` with a leading dot
/// (`:170`). Downloads land where the add puts them, so the dotless form is the
/// one both sides use here and the dotted one is treated as the Sonarr bug it
/// is.
fn category_root(config: &TriblerConfig, settings: &TriblerSettings) -> Option<String> {
    if config.category.is_empty() {
        return None;
    }
    let save_as = settings.save_as();
    if save_as.is_empty() {
        return None;
    }
    Some(format!("{save_as}/{}", config.category))
}

/// The roots downloads can appear under, most specific first.
///
/// Sonarr reports exactly one (`GetStatus`), and ignores its own `TvDirectory`
/// setting while doing it. Scryer's `remote_output_roots` is a list, so the
/// effective add root and Tribler's own save-as root can both be reported and
/// remote path mapping has both prefixes to match against.
fn output_roots(config: &TriblerConfig, settings: &TriblerSettings) -> Vec<String> {
    let mut roots = Vec::new();
    if !config.directory.is_empty() {
        roots.push(config.directory.trim_end_matches('/').to_string());
    } else if let Some(root) = category_root(config, settings) {
        roots.push(root);
    }
    let save_as = settings.save_as();
    if !save_as.is_empty() && !roots.iter().any(|root| root == save_as) {
        roots.push(save_as.to_string());
    }
    roots
}

/// Tribler has no category field on a download, so the only honest way to report
/// one is to recognise the directory the plugin puts categorised downloads in.
/// Reporting the configured category on every download would label torrents this
/// Scryer never grabbed.
///
/// The configured casing is reported and the comparison is case-insensitive,
/// per the fleet's category-casing rule.
fn item_category(
    config: &TriblerConfig,
    settings: &TriblerSettings,
    download: &TriblerDownload,
) -> Option<String> {
    let root = category_root(config, settings)?;
    let destination = download.destination.trim_end_matches('/');
    let matches = destination.eq_ignore_ascii_case(&root)
        || destination
            .to_ascii_lowercase()
            .starts_with(&format!("{}/", root.to_ascii_lowercase()));
    matches.then(|| config.category.clone())
}

/// Sonarr: single file → destination + that file's name, otherwise destination +
/// the item title (`TriblerDownloadClient.cs:74-83`). Tribler makes the same
/// distinction on its side: a multi-file torrent's file names are relative to the
/// torrent's own directory, a single-file torrent's are not
/// (`downloads_endpoint.py::get_files_info_json`).
fn output_path(download: &TriblerDownload, files: &[String]) -> String {
    if files.len() == 1 {
        join_path(&download.destination, &files[0])
    } else {
        join_path(&download.destination, &download.name)
    }
}

// ---------------------------------------------------------------------------
// Item mapping
// ---------------------------------------------------------------------------

fn torrent_to_item(
    config: &TriblerConfig,
    settings: &TriblerSettings,
    download: TriblerDownload,
    now: i64,
) -> Result<PluginDownloadItem, PluginError> {
    // One download whose file list cannot be read (removed between the
    // listing and this call, or still resolving) must not fail the whole
    // queue poll; the path falls back to destination + name, and the next
    // poll retries because empty lists are never cached.
    let files = match download_files(config, &download.infohash) {
        Ok(files) => files,
        Err(error) => {
            warn_log!(
                "Tribler did not list the files of {}: {}",
                download.infohash,
                error.public_message
            );
            Vec::new()
        }
    };
    let output_path = output_path(&download, &files);
    let size = download.size.unwrap_or_default();
    let progress = download.progress();
    let remaining = ((size as f64) * (1.0 - progress)).round().max(0.0) as i64;
    let state = map_state(&download);
    let hash = normalize_hash(&download.infohash);
    let can_remove = derive_can_remove(&download, settings.download_defaults(), now);
    let can_move_files = Some(is_data_complete(&download));
    let message = state_message(&download);
    let category = item_category(config, settings, &download);
    let completed_at = download.seeding_started_at().and_then(unix_to_rfc3339);
    let seed_time_seconds = download
        .seeding_started_at()
        .map(|started| (now - started).max(0));
    Ok(PluginDownloadItem {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash.clone()),
        title: download.name.clone(),
        state,
        message,
        category,
        remote_output_path: non_empty(output_path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(hash),
            save_path: non_empty(download.destination.clone()),
            content_paths: non_empty(output_path).into_iter().collect(),
            uploaded_bytes: download.uploaded_bytes(),
            downloaded_bytes: download.downloaded_bytes(),
            upload_rate_bytes_per_second: download.speed_up.map(|value| value as i64),
            download_rate_bytes_per_second: download.speed_down.map(|value| value as i64),
            seed_ratio: download.seed_ratio(),
            seed_time_seconds,
            // Tribler always writes payloads unencrypted; Sonarr says the same
            // explicitly (`TriblerDownloadClient.cs:108`).
            is_encrypted: Some(false),
            // `GET /downloads` never reports the metainfo `private` flag, so
            // this stays unknown rather than being guessed at.
            is_private: None,
            raw_status: download.status.clone(),
            status_reason: download.error_message().map(str::to_string),
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(size),
        remaining_size_bytes: Some(remaining),
        // Sonarr clamps an ETA of a year or more to a year and a negative one to
        // zero (`TriblerDownloadClient.cs:89-103`).
        eta_seconds: download
            .eta
            .map(|value| value.clamp(0.0, 31_536_000.0) as i64),
        progress_percent: Some((progress * 100.0).round().clamp(0.0, 100.0) as u8),
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files,
        can_remove,
        removed: Some(false),
        raw_state: download.status,
        completed_at,
    })
}

fn torrent_to_completed(
    config: &TriblerConfig,
    settings: &TriblerSettings,
    download: TriblerDownload,
) -> Result<PluginCompletedDownload, PluginError> {
    let files = download_files(config, &download.infohash)?;
    let output_path = output_path(&download, &files);
    let hash = normalize_hash(&download.infohash);
    let category = item_category(config, settings, &download);
    let completed_at = download.seeding_started_at().and_then(unix_to_rfc3339);
    Ok(PluginCompletedDownload {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash),
        name: download.name,
        dest_dir: output_path.clone(),
        category,
        // Sonarr's own rule: one file means the path *is* the file, anything
        // else is the torrent's directory. A dotted torrent name is still a
        // directory.
        output_kind: Some(if files.len() == 1 {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: non_empty(output_path).into_iter().collect(),
        size_bytes: download.size,
        completed_at,
        parameters: Vec::new(),
        release_name: None,
    })
}

/// A magnet that has not resolved its metadata yet has no size, and Sonarr skips
/// it for exactly that reason (`TriblerDownloadClient.cs:57-61`).
fn is_visible_download(download: &TriblerDownload) -> bool {
    download.size.unwrap_or_default() > 0
}

/// Sonarr's status switch (`TriblerDownloadClient.cs:110-148`), corrected against
/// Tribler's own `DownloadStatus` enum and widened where Scryer can say more.
///
/// The status strings are `DownloadStatus.<member>.name`
/// (`downloads_endpoint.py::get_downloads`), and the members changed over time:
///
/// - `WAITING_FOR_HASHCHECK` is the spelling in every Tribler from `v7.14.0` to
///   `v8.4.3`. Sonarr's enum says `WAITING4HASHCHECK`
///   (`TriblerDownloadClientApi.cs:11`), which matches nothing Tribler emits; it
///   is accepted here as well, but only as a legacy alias.
/// - `CIRCUITS` existed up to `v8.0.7` and was replaced by `LOADING` in `v8.1.0`.
/// - `MOVING` and `QUEUED` were added in `v8.2.0`.
///
/// `status_code` is deliberately not used: the enum's *values* were renumbered
/// between 7.x and 8.x and `8` means `CIRCUITS` on `v8.0.7` but `LOADING` from
/// `v8.1.0`, so only the name is stable.
///
/// Divergences from Sonarr, all in the "strictly more informative" direction:
/// hash checking is `Verifying` rather than `Downloading` (Sonarr `:112-117`);
/// `SEEDING` is `Seeding` rather than `Completed` (Sonarr `:123-125`), which the
/// adapter maps back to the completed queue state; `QUEUED` joins Sonarr's
/// `Queued` group; `MOVING` is `Extracting`, the nearest state for "the payload
/// is being relocated and is not stable yet".
fn map_state(download: &TriblerDownload) -> DownloadItemState {
    if download.error_message().is_some() {
        return DownloadItemState::Warning;
    }
    match download.status() {
        "HASHCHECKING" | "WAITING_FOR_HASHCHECK" | "WAITING4HASHCHECK" => {
            DownloadItemState::Verifying
        }
        "DOWNLOADING" | "CIRCUITS" | "EXIT_NODES" | "LOADING" => DownloadItemState::Downloading,
        "METADATA" | "ALLOCATING_DISKSPACE" | "QUEUED" => DownloadItemState::Queued,
        "MOVING" => DownloadItemState::Extracting,
        "SEEDING" => DownloadItemState::Seeding,
        "STOPPED" if download.progress() < 1.0 => DownloadItemState::Paused,
        "STOPPED" => DownloadItemState::Completed,
        "STOPPED_ON_ERROR" => DownloadItemState::Failed,
        // An unrecognised status keeps polling as `Downloading` and says why,
        // which is both Sonarr's default arm (`:130-135`) and Scryer's
        // unknown-state rule.
        _ => DownloadItemState::Downloading,
    }
}

fn state_message(download: &TriblerDownload) -> Option<String> {
    if let Some(error) = download.error_message() {
        return Some(error.to_string());
    }
    is_unknown_status(download.status())
        .then(|| format!("Unknown download state: {}", download.status()))
}

fn is_unknown_status(status: &str) -> bool {
    !matches!(
        status,
        "HASHCHECKING"
            | "WAITING_FOR_HASHCHECK"
            | "WAITING4HASHCHECK"
            | "DOWNLOADING"
            | "CIRCUITS"
            | "EXIT_NODES"
            | "LOADING"
            | "METADATA"
            | "ALLOCATING_DISKSPACE"
            | "QUEUED"
            | "MOVING"
            | "SEEDING"
            | "STOPPED"
            | "STOPPED_ON_ERROR"
    )
}

/// Honest `can_remove` for Tribler — the tri-state form of Sonarr's
/// `HasReachedSeedLimit` (`TriblerDownloadClient.cs:180-219`).
///
/// Tribler enforces one goal, the global `seeding_mode` in its libtorrent
/// download defaults, and stops a download itself the moment that goal is met
/// (`download.py::update_lt_status`). So `STOPPED` on a complete download is the
/// client's own proof that the obligation is discharged, and anything short of
/// that is at most an unmet limit.
///
/// Two corrections against Tribler's source, where Sonarr is wrong:
///
/// - `time` mode compares the *seeding* time to the goal
///   (`state.get_seeding_time() >= seeding_time`), not the time since the
///   torrent was added. Sonarr uses `TimeAdded + SeedingTime < Now` (`:205-210`),
///   which declares a torrent seeded-out the moment it finishes if it spent
///   longer than the goal downloading. Seeding starts at `time_finished`, which
///   Tribler reports from `v8.3.0`; before that the elapsed seeding time is
///   unknowable and this reports `None`.
/// - the ratio goal is per download from `v8.4.1`
///   (`download.py::get_seeding_ratio`, surfaced as the `seeding_ratio` field),
///   so the download's own target is preferred over the global default.
fn derive_can_remove(
    download: &TriblerDownload,
    defaults: &DownloadDefaults,
    now: i64,
) -> Option<bool> {
    if !is_data_complete(download) {
        return Some(false);
    }
    let is_stopped = download.status() == "STOPPED";
    match defaults.seeding_mode.as_deref() {
        // No seeding obligation at all.
        Some("never") => Some(true),
        // Seed indefinitely: the goal can never be reached.
        Some("forever") => Some(false),
        Some("ratio") => {
            let target = download.seeding_ratio.or(defaults.seeding_ratio);
            match download.seed_ratio().zip(target) {
                Some((actual, target)) if actual >= target => is_stopped.then_some(true),
                Some(_) => Some(false),
                None => None,
            }
        }
        Some("time") => match download.seeding_started_at().zip(defaults.seeding_time) {
            Some((started, goal)) if now.saturating_sub(started) >= goal as i64 => {
                is_stopped.then_some(true)
            }
            Some(_) => Some(false),
            None => None,
        },
        _ => None,
    }
}

/// Whether the payload is fully downloaded and stable on disk.
///
/// `MOVING` is the one complete-looking status that is not stable: Tribler
/// relocates a finished download to `completed_dir` and only then updates
/// `destination` (`download.py::on_torrent_finished_alert` → `move_storage`).
/// The hash-check and allocation statuses report progress against a different
/// denominator entirely (`download_state.py::get_progress`), so they are not
/// evidence of a complete payload either.
fn is_data_complete(download: &TriblerDownload) -> bool {
    match download.status() {
        "MOVING"
        | "HASHCHECKING"
        | "WAITING_FOR_HASHCHECK"
        | "WAITING4HASHCHECK"
        | "ALLOCATING_DISKSPACE"
        | "METADATA" => false,
        "SEEDING" => true,
        _ => download.progress() >= 1.0,
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

struct TriblerHttpResponse {
    status: u16,
    body: String,
    location: Option<String>,
}

fn user_agent() -> String {
    format!("scryer-tribler-plugin/{}", env!("CARGO_PKG_VERSION"))
}

fn send(
    config: &TriblerConfig,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    content_type: &str,
) -> Result<TriblerHttpResponse, PluginError> {
    let request = HttpRequest::new(format!(
        "{}{}{}",
        config.api_root.trim_end_matches('/'),
        if path.starts_with('/') { "" } else { "/" },
        path
    ))
    .with_method(method)
    .with_header("Accept", "application/json")
    .with_header("Content-Type", content_type)
    // Tribler accepts the key as the `X-Api-Key` header, a `key` query
    // parameter or an `api_key` cookie (`rest_manager.py::ApiKeyMiddleware`);
    // the header is the one its own OpenAPI security scheme documents.
    .with_header("X-Api-Key", &config.api_key)
    .with_header("User-Agent", user_agent());
    let response = http::request::<Vec<u8>>(&request, body)
        .map_err(|error| classify_transport_error(&error.to_string()))?;
    Ok(TriblerHttpResponse {
        status: response.status_code(),
        body: String::from_utf8_lossy(&response.body()).to_string(),
        location: response.header("Location").map(str::to_string),
    })
}

fn request_json<T: DeserializeOwned>(
    config: &TriblerConfig,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T, PluginError> {
    let payload = body
        .map(|body| serde_json::to_vec(&body))
        .transpose()
        .map_err(encoding_error)?;
    let response = send(config, method, path, payload, "application/json")?;
    if let Some(error) = classify_http_status(&response) {
        return Err(error);
    }
    parse_json(&response.body)
}

/// A control call against a download Tribler no longer has.
///
/// Tribler answers `404` both for an unknown info hash
/// (`downloads_endpoint.py::delete_download` → `return_404`) and for an unknown
/// route, and neither is worth failing an import over: a torrent that is already
/// gone is the outcome the caller wanted. Sonarr swallows the same case, and
/// Scryer's contract makes it an explicit warn-and-`Ok(())`.
///
fn request_ignoring_missing(
    config: &TriblerConfig,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(), PluginError> {
    let payload = body
        .map(|body| serde_json::to_vec(&body))
        .transpose()
        .map_err(encoding_error)?;
    let response = send(config, method, path, payload, "application/json")?;
    if response.status == 404 {
        warn_log!(
            "Tribler no longer knows about this download; treating {method} {path} as already done."
        );
        return Ok(());
    }
    if let Some(error) = classify_http_status(&response) {
        return Err(error);
    }
    Ok(())
}

fn parse_json<T: DeserializeOwned>(body: &str) -> Result<T, PluginError> {
    serde_json::from_str(body).map_err(|error| {
        detailed_error(
            PluginErrorCode::Temporary,
            "Tribler returned a response Scryer could not read.",
            format!("{error}: {}", truncate(body)),
        )
    })
}

fn encoding_error(error: serde_json::Error) -> PluginError {
    detailed_error(
        PluginErrorCode::Permanent,
        "Scryer could not encode a request for Tribler.",
        error.to_string(),
    )
}

fn get_settings(config: &TriblerConfig) -> Result<TriblerSettings, PluginError> {
    let response: TriblerSettingsResponse = request_json(config, "GET", "/settings", None)?;
    Ok(response.settings)
}

fn get_downloads(config: &TriblerConfig) -> Result<Vec<TriblerDownload>, PluginError> {
    let response: DownloadsResponse = request_json(config, "GET", "/downloads", None)?;
    Ok(response.downloads)
}

/// Tribler's own version, from `GET /versioning/versions/current`
/// (`versioning_endpoint.py`, present from `v8.0.7` through `v8.4.3`), which
/// answers `{"version": "8.4.3"}` — or the literal `"git"` for a source build.
///
/// Sonarr does not report a Tribler version at all. This is best effort: an
/// instance without the versioning component simply has no version to show, and
/// that must not turn a healthy client's status into an error.
fn server_version(config: &TriblerConfig) -> Option<String> {
    let response: VersionResponse =
        request_json(config, "GET", "/versioning/versions/current", None)
            .inspect_err(|error| {
                debug_log!("Tribler did not report a version: {}", error.public_message);
            })
            .ok()?;
    let version = response.version.trim();
    (!version.is_empty() && version != "git").then(|| version.to_string())
}

/// The substance of Sonarr's `DownloadClientTriblerProviderMessage`
/// ("The tribler integration is highly experimental. Tested against {clientName}
/// version {clientVersionRange}." with `8.0.7`, `en.json:577`), plus the one
/// thing Sonarr cannot say because it never reads a version: whether the
/// instance in front of us predates the API this plugin reads.
fn provider_warnings(version: Option<&str>) -> Vec<String> {
    let mut warnings = vec![format!(
        "Tribler support is experimental. Tested against Tribler {MINIMUM_TESTED_VERSION} to {NEWEST_TESTED_VERSION}."
    )];
    if let Some(version) = version
        && let Some(major) = major_version(version)
        && major < 8
    {
        warnings.push(format!(
            "Tribler {version} predates the 8.x REST API; completion times, per-download seed ratios and category folders may be unavailable."
        ));
    }
    warnings
}

fn major_version(version: &str) -> Option<u32> {
    version
        .trim()
        .trim_start_matches('v')
        .split(['.', '-'])
        .next()?
        .parse()
        .ok()
}

// ---------------------------------------------------------------------------
// Per-download file cache
// ---------------------------------------------------------------------------

fn files_var_key(hash: &str) -> String {
    format!("tribler.files.{hash}")
}

/// The file names of one download, cached for the life of the plugin instance.
///
/// Sonarr calls `GET /downloads/{infohash}/files` for every visible download on
/// every `GetItems` and leaves a "some concurrency could make this faster"
/// comment about it (`TriblerDownloadClient.cs:71-72`). Scryer polls both
/// `list_queue` and `list_completed`, so the naive port cost two files requests
/// per download per poll, forever. The only things read from that response are
/// the file count and, for a single-file torrent, the one name — neither of
/// which can change for a given info hash — so one request per download per
/// instance is exactly equivalent.
fn download_files(config: &TriblerConfig, hash: &str) -> Result<Vec<String>, PluginError> {
    let key = files_var_key(hash);
    if let Some(cached) = var::get::<Vec<String>>(&key).ok().flatten()
        && !cached.is_empty()
    {
        return Ok(cached);
    }
    let response: FilesResponse =
        request_json(config, "GET", &format!("/downloads/{hash}/files"), None)?;
    let names = response
        .files
        .into_iter()
        .map(|file| file.name)
        .filter(|name| !name.trim().is_empty())
        .collect::<Vec<_>>();
    if !names.is_empty() {
        let _ = var::set(&key, &names);
    }
    Ok(names)
}

fn forget_download_state(hash: &str) {
    let _ = var::remove(files_var_key(hash));
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
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

/// Howard's `civil_from_days`, the same conversion the transmission client uses.
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

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches(['/', '\\']), name)
    }
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

/// Percent-encode a query string. Only the torrent-bytes add needs one, and it
/// carries a filesystem path, so everything outside the unreserved set is
/// escaped.
fn encode_query(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn download(status: &str, progress: f64, ratio: f64) -> TriblerDownload {
        TriblerDownload {
            name: "Movie".to_string(),
            progress: Some(progress),
            infohash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            status: Some(status.to_string()),
            all_time_ratio: Some(ratio),
            time_added: Some(NOW - 3_600),
            size: Some(1_000),
            destination: "/downloads".to_string(),
            ..TriblerDownload::default()
        }
    }

    fn defaults(mode: Option<&str>, ratio: Option<f64>, time: Option<f64>) -> DownloadDefaults {
        DownloadDefaults {
            save_as: "/downloads".to_string(),
            seeding_mode: mode.map(str::to_string),
            seeding_ratio: ratio,
            seeding_time: time,
        }
    }

    fn config() -> TriblerConfig {
        TriblerConfig {
            api_root: "http://localhost:20100/api".to_string(),
            api_key: "key".to_string(),
            anonymity_level: Some(1),
            safe_seeding: true,
            ..TriblerConfig::default()
        }
    }

    fn settings(save_as: &str) -> TriblerSettings {
        TriblerSettings {
            libtorrent: LibTorrent {
                download_defaults: DownloadDefaults {
                    save_as: save_as.to_string(),
                    ..DownloadDefaults::default()
                },
            },
            download_defaults: DownloadDefaults::default(),
        }
    }

    // -----------------------------------------------------------------------
    // can_remove
    // -----------------------------------------------------------------------

    #[test]
    fn can_remove_is_false_while_downloading() {
        assert_eq!(
            derive_can_remove(
                &download("DOWNLOADING", 0.4, 0.0),
                &defaults(Some("ratio"), Some(1.0), None),
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_ratio() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 0.4),
                &defaults(Some("ratio"), Some(2.0), None),
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_true_once_tribler_stopped_a_download_at_its_ratio() {
        assert_eq!(
            derive_can_remove(
                &download("STOPPED", 1.0, 2.5),
                &defaults(Some("ratio"), Some(2.0), None),
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_true_when_tribler_never_seeds() {
        assert_eq!(
            derive_can_remove(
                &download("STOPPED", 1.0, 0.0),
                &defaults(Some("never"), None, None),
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_false_when_tribler_seeds_forever() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 9.0),
                &defaults(Some("forever"), None, None),
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_unknown_without_a_recognised_seeding_mode() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 9.0),
                &defaults(None, None, None),
                NOW
            ),
            None
        );
    }

    #[test]
    fn can_remove_is_unknown_when_the_ratio_goal_lacks_a_target() {
        assert_eq!(
            derive_can_remove(
                &download("STOPPED", 1.0, 9.0),
                &defaults(Some("ratio"), None, None),
                NOW
            ),
            None
        );
    }

    #[test]
    fn met_goal_that_tribler_has_not_stopped_yet_is_unknown() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 9.0),
                &defaults(Some("ratio"), Some(2.0), None),
                NOW
            ),
            None
        );
    }

    /// `download.py::get_seeding_ratio` prefers the download's own goal over the
    /// global default, and `v8.4.1` surfaces the effective value as
    /// `seeding_ratio`.
    #[test]
    fn a_per_download_ratio_goal_beats_the_global_default() {
        let mut torrent = download("STOPPED", 1.0, 1.5);
        torrent.seeding_ratio = Some(1.0);
        assert_eq!(
            derive_can_remove(&torrent, &defaults(Some("ratio"), Some(9.0), None), NOW),
            Some(true)
        );

        let mut torrent = download("SEEDING", 1.0, 1.5);
        torrent.seeding_ratio = Some(4.0);
        assert_eq!(
            derive_can_remove(&torrent, &defaults(Some("ratio"), Some(1.0), None), NOW),
            Some(false)
        );
    }

    /// Tribler measures seeding time from completion, not from when the torrent
    /// was added. A torrent added long ago but finished a minute ago has not met
    /// an hour-long goal, which is exactly the case Sonarr gets wrong
    /// (`TriblerDownloadClient.cs:205-210`).
    #[test]
    fn a_time_goal_is_measured_from_completion_not_from_when_the_torrent_was_added() {
        let mut torrent = download("SEEDING", 1.0, 0.0);
        torrent.time_added = Some(NOW - 100_000);
        torrent.time_finished = Some(NOW - 60);
        assert_eq!(
            derive_can_remove(&torrent, &defaults(Some("time"), None, Some(3_600.0)), NOW),
            Some(false)
        );
    }

    #[test]
    fn a_met_time_goal_that_tribler_stopped_discharges_the_obligation() {
        let mut torrent = download("STOPPED", 1.0, 0.0);
        torrent.time_finished = Some(NOW - 7_200);
        assert_eq!(
            derive_can_remove(&torrent, &defaults(Some("time"), None, Some(3_600.0)), NOW),
            Some(true)
        );
    }

    /// Tribler before `v8.3.0` does not report `time_finished`, so the elapsed
    /// seeding time cannot be known and the core has to decide.
    #[test]
    fn a_time_goal_is_unknown_without_a_completion_timestamp() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 0.0),
                &defaults(Some("time"), None, Some(3_600.0)),
                NOW
            ),
            None
        );
    }

    #[test]
    fn a_download_being_moved_to_its_completed_directory_cannot_be_removed_yet() {
        assert_eq!(
            derive_can_remove(
                &download("MOVING", 1.0, 9.0),
                &defaults(Some("never"), None, None),
                NOW
            ),
            Some(false)
        );
    }

    // -----------------------------------------------------------------------
    // Data completeness
    // -----------------------------------------------------------------------

    #[test]
    fn data_completeness_covers_seeding_and_finished_downloads() {
        assert!(is_data_complete(&download("SEEDING", 0.999, 0.0)));
        assert!(is_data_complete(&download("STOPPED", 1.0, 0.0)));
        assert!(!is_data_complete(&download("DOWNLOADING", 0.5, 0.0)));
    }

    #[test]
    fn data_is_not_complete_while_it_is_being_checked_or_relocated() {
        assert!(!is_data_complete(&download("MOVING", 1.0, 0.0)));
        assert!(!is_data_complete(&download("HASHCHECKING", 1.0, 0.0)));
        assert!(!is_data_complete(&download(
            "WAITING_FOR_HASHCHECK",
            1.0,
            0.0
        )));
        assert!(!is_data_complete(&download(
            "ALLOCATING_DISKSPACE",
            1.0,
            0.0
        )));
    }

    // -----------------------------------------------------------------------
    // Status table
    // -----------------------------------------------------------------------

    #[test]
    fn the_status_table_matches_triblers_download_status_enum() {
        for (status, expected) in [
            ("WAITING_FOR_HASHCHECK", DownloadItemState::Verifying),
            ("WAITING4HASHCHECK", DownloadItemState::Verifying),
            ("HASHCHECKING", DownloadItemState::Verifying),
            ("DOWNLOADING", DownloadItemState::Downloading),
            ("CIRCUITS", DownloadItemState::Downloading),
            ("EXIT_NODES", DownloadItemState::Downloading),
            ("LOADING", DownloadItemState::Downloading),
            ("METADATA", DownloadItemState::Queued),
            ("ALLOCATING_DISKSPACE", DownloadItemState::Queued),
            ("QUEUED", DownloadItemState::Queued),
            ("MOVING", DownloadItemState::Extracting),
            ("SEEDING", DownloadItemState::Seeding),
            ("STOPPED_ON_ERROR", DownloadItemState::Failed),
        ] {
            assert_eq!(
                map_state(&download(status, 1.0, 0.0)),
                expected,
                "status {status}"
            );
        }
    }

    #[test]
    fn a_stopped_download_is_paused_until_its_data_is_complete() {
        assert_eq!(
            map_state(&download("STOPPED", 0.5, 0.0)),
            DownloadItemState::Paused
        );
        assert_eq!(
            map_state(&download("STOPPED", 1.0, 0.0)),
            DownloadItemState::Completed
        );
    }

    #[test]
    fn an_error_string_is_a_warning_with_the_message_tribler_gave() {
        let mut torrent = download("DOWNLOADING", 0.5, 0.0);
        torrent.error = Some("no space left on device".to_string());
        assert_eq!(map_state(&torrent), DownloadItemState::Warning);
        assert_eq!(
            state_message(&torrent).as_deref(),
            Some("no space left on device")
        );
    }

    /// Tribler serialises "no error" as `""`, not `null`.
    #[test]
    fn an_empty_error_string_is_not_an_error() {
        let mut torrent = download("DOWNLOADING", 0.5, 0.0);
        torrent.error = Some(String::new());
        assert_eq!(map_state(&torrent), DownloadItemState::Downloading);
        assert_eq!(state_message(&torrent), None);
    }

    /// An unrecognised status keeps polling, and says why.
    #[test]
    fn an_unknown_status_keeps_polling_and_reports_it() {
        let torrent = download("TELEPORTING", 0.5, 0.0);
        assert_eq!(map_state(&torrent), DownloadItemState::Downloading);
        assert_eq!(
            state_message(&torrent).as_deref(),
            Some("Unknown download state: TELEPORTING")
        );
    }

    // -----------------------------------------------------------------------
    // Paths, roots and categories
    // -----------------------------------------------------------------------

    /// The add and the reported root have to name the same directory. Sonarr's
    /// do not (`:170` dots the category, `:265` does not).
    #[test]
    fn the_category_root_is_the_directory_downloads_are_added_to() {
        let config = TriblerConfig {
            category: "tv".to_string(),
            ..config()
        };
        let settings = settings("/data/downloads/");
        assert_eq!(
            category_root(&config, &settings).as_deref(),
            Some("/data/downloads/tv")
        );
        assert_eq!(
            output_roots(&config, &settings),
            vec![
                "/data/downloads/tv".to_string(),
                "/data/downloads".to_string()
            ]
        );
    }

    #[test]
    fn a_configured_directory_is_reported_as_the_output_root() {
        let config = TriblerConfig {
            directory: "/mnt/tv/".to_string(),
            ..config()
        };
        assert_eq!(
            output_roots(&config, &settings("/data/downloads")),
            vec!["/mnt/tv".to_string(), "/data/downloads".to_string()]
        );
    }

    #[test]
    fn without_a_category_or_directory_the_save_as_root_is_the_only_root() {
        assert_eq!(
            output_roots(&config(), &settings("/data/downloads")),
            vec!["/data/downloads".to_string()]
        );
    }

    /// Tribler 7.x keeps `download_defaults` at the top level of the settings
    /// payload; 8.x nests it under `libtorrent`.
    #[test]
    fn the_save_as_root_is_read_from_either_settings_layout() {
        let eight: TriblerSettings = serde_json::from_str(
            r#"{"libtorrent":{"download_defaults":{"saveas":"/eight","seeding_mode":"ratio"}}}"#,
        )
        .expect("8.x settings");
        assert_eq!(eight.save_as(), "/eight");
        assert_eq!(
            eight.download_defaults().seeding_mode.as_deref(),
            Some("ratio")
        );

        let seven: TriblerSettings = serde_json::from_str(
            r#"{"download_defaults":{"saveas":"/seven/","seeding_mode":"time"}}"#,
        )
        .expect("7.x settings");
        assert_eq!(seven.save_as(), "/seven");
        assert_eq!(
            seven.download_defaults().seeding_mode.as_deref(),
            Some("time")
        );
    }

    #[test]
    fn a_single_file_download_reports_the_file_and_a_multi_file_one_reports_the_folder() {
        let torrent = download("SEEDING", 1.0, 0.0);
        assert_eq!(
            output_path(&torrent, &["Movie.2026.mkv".to_string()]),
            "/downloads/Movie.2026.mkv"
        );
        assert_eq!(
            output_path(&torrent, &["a.mkv".to_string(), "b.mkv".to_string()]),
            "/downloads/Movie"
        );
    }

    #[test]
    fn the_configured_category_is_reported_only_for_downloads_inside_its_folder() {
        let config = TriblerConfig {
            category: "TV".to_string(),
            ..config()
        };
        let settings = settings("/data/downloads");

        let mut inside = download("SEEDING", 1.0, 0.0);
        inside.destination = "/data/downloads/tv".to_string();
        assert_eq!(
            item_category(&config, &settings, &inside).as_deref(),
            Some("TV"),
            "the client's own casing is reported, matching is case-insensitive"
        );

        let mut nested = download("SEEDING", 1.0, 0.0);
        nested.destination = "/data/downloads/TV/Show".to_string();
        assert_eq!(
            item_category(&config, &settings, &nested).as_deref(),
            Some("TV")
        );

        let mut outside = download("SEEDING", 1.0, 0.0);
        outside.destination = "/data/downloads/movies".to_string();
        assert_eq!(item_category(&config, &settings, &outside), None);
    }

    #[test]
    fn without_a_category_no_category_is_reported() {
        let mut torrent = download("SEEDING", 1.0, 0.0);
        torrent.destination = "/data/downloads".to_string();
        assert_eq!(
            item_category(&config(), &settings("/data/downloads"), &torrent),
            None
        );
    }

    // -----------------------------------------------------------------------
    // Item mapping
    // -----------------------------------------------------------------------

    #[test]
    fn a_completion_timestamp_becomes_completed_at() {
        assert_eq!(
            unix_to_rfc3339(1_700_000_000).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert_eq!(unix_to_rfc3339(0), None);

        let mut torrent = download("SEEDING", 1.0, 0.0);
        torrent.time_finished = Some(0);
        assert_eq!(torrent.seeding_started_at(), None);
        torrent.time_finished = Some(1_700_000_000);
        assert_eq!(torrent.seeding_started_at(), Some(1_700_000_000));
    }

    /// 7.x names for the transfer counters, which Tribler renamed in 8.x.
    #[test]
    fn seven_x_transfer_counters_are_read_as_fallbacks() {
        let seven: TriblerDownload =
            serde_json::from_str(r#"{"ratio":1.25,"total_up":10,"total_down":20}"#)
                .expect("7.x download");
        assert_eq!(seven.seed_ratio(), Some(1.25));
        assert_eq!(seven.uploaded_bytes(), Some(10));
        assert_eq!(seven.downloaded_bytes(), Some(20));

        let eight: TriblerDownload = serde_json::from_str(
            r#"{"all_time_ratio":2.5,"all_time_upload":30,"all_time_download":40,"total_down":1}"#,
        )
        .expect("8.x download");
        assert_eq!(eight.seed_ratio(), Some(2.5));
        assert_eq!(eight.uploaded_bytes(), Some(30));
        assert_eq!(eight.downloaded_bytes(), Some(40));
    }

    #[test]
    fn a_metadata_only_magnet_is_not_listed() {
        let mut torrent = download("METADATA", 0.0, 0.0);
        torrent.size = Some(0);
        assert!(!is_visible_download(&torrent));
        assert!(is_visible_download(&download("DOWNLOADING", 0.1, 0.0)));
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    #[test]
    fn a_valid_configuration_passes() {
        assert!(validate_config(&config()).is_ok());
    }

    #[test]
    fn an_api_key_is_required() {
        let config = TriblerConfig {
            api_key: String::new(),
            ..config()
        };
        assert_eq!(
            validate_config(&config).unwrap_err().code,
            PluginErrorCode::InvalidConfig
        );
    }

    #[test]
    fn a_url_base_may_not_be_a_url() {
        assert!(is_valid_url_base(""));
        assert!(is_valid_url_base("/tribler"));
        assert!(is_valid_url_base("tribler"));
        assert!(!is_valid_url_base("http://localhost:20100"));
        assert!(!is_valid_url_base("/HTTPS://tribler.example"));
    }

    #[test]
    fn a_category_allows_only_letters_and_dashes_with_an_optional_leading_dot() {
        assert!(is_valid_category(""));
        assert!(is_valid_category("tv"));
        assert!(is_valid_category("TV"));
        assert!(is_valid_category(".tv-shows"));
        assert!(!is_valid_category("tv2"));
        assert!(!is_valid_category("tv shows"));
        assert!(!is_valid_category("tv/shows"));
    }

    #[test]
    fn a_category_and_a_directory_cannot_both_be_set() {
        let config = TriblerConfig {
            category: "tv".to_string(),
            directory: "/mnt/tv".to_string(),
            ..config()
        };
        let error = validate_config(&config).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(
            error
                .public_message
                .contains("Cannot use Category and Directory")
        );
    }

    #[test]
    fn with_both_set_the_directory_wins_and_adds_are_not_refused() {
        let config = TriblerConfig {
            category: "tv".to_string(),
            directory: "/mnt/tv".to_string(),
            ..config()
        };
        assert!(validate_add_config(&config).is_ok());
        let settings = settings("/downloads");
        assert_eq!(output_roots(&config, &settings)[0], "/mnt/tv");
    }

    #[test]
    fn a_multi_file_download_with_a_dotted_name_is_still_a_directory() {
        let mut torrent = download("SEEDING", 1.0, 0.0);
        torrent.name = "Show.S01.1080p.WEB".to_string();
        assert_eq!(
            output_path(&torrent, &["a.mkv".to_string(), "b.mkv".to_string()]),
            "/downloads/Show.S01.1080p.WEB"
        );
    }

    #[test]
    fn an_anonymity_level_must_be_a_non_negative_number() {
        let unparseable = TriblerConfig {
            anonymity_level: None,
            anonymity_level_raw: "lots".to_string(),
            ..config()
        };
        assert_eq!(
            validate_config(&unparseable).unwrap_err().code,
            PluginErrorCode::InvalidConfig
        );

        let negative = TriblerConfig {
            anonymity_level: Some(-1),
            anonymity_level_raw: "-1".to_string(),
            ..config()
        };
        assert_eq!(
            validate_config(&negative).unwrap_err().code,
            PluginErrorCode::InvalidConfig
        );
    }

    /// Tribler answers HTTP 400 for this combination, so it is refused before
    /// the request goes out.
    #[test]
    fn anonymous_downloading_requires_safe_seeding() {
        let anonymous = TriblerConfig {
            anonymity_level: Some(2),
            safe_seeding: false,
            ..config()
        };
        assert_eq!(
            validate_config(&anonymous).unwrap_err().code,
            PluginErrorCode::InvalidConfig
        );

        let plain = TriblerConfig {
            anonymity_level: Some(0),
            safe_seeding: false,
            ..config()
        };
        assert!(validate_config(&plain).is_ok());
    }

    // -----------------------------------------------------------------------
    // Errors
    // -----------------------------------------------------------------------

    fn response(status: u16, body: &str) -> TriblerHttpResponse {
        TriblerHttpResponse {
            status,
            body: body.to_string(),
            location: None,
        }
    }

    /// Sonarr's two distinctions (`TriblerDownloadClientProxy.cs:62-70`), plus
    /// the configuration faults Scryer can see because the host does not follow
    /// redirects.
    #[test]
    fn http_failures_carry_the_right_error_code() {
        assert!(classify_http_status(&response(200, "{}")).is_none());

        let unauthorized = classify_http_status(&response(
            401,
            r#"{"error":{"handled":true,"message":"Unauthorized access"}}"#,
        ))
        .expect("401 is an error");
        assert_eq!(unauthorized.code, PluginErrorCode::AuthFailed);
        assert_eq!(
            unauthorized.debug_message.as_deref(),
            Some("Unauthorized access")
        );

        assert_eq!(
            classify_http_status(&response(404, "{}")).unwrap().code,
            PluginErrorCode::InvalidConfig
        );
        assert_eq!(
            classify_http_status(&response(
                400,
                r#"{"error":{"handled":true,"message":"uri parameter missing"}}"#
            ))
            .unwrap()
            .code,
            PluginErrorCode::Permanent
        );
        assert_eq!(
            classify_http_status(&response(503, "busy")).unwrap().code,
            PluginErrorCode::Temporary
        );

        let redirected = TriblerHttpResponse {
            status: 302,
            body: String::new(),
            location: Some("/login".to_string()),
        };
        let error = classify_http_status(&redirected).expect("302 is an error");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("/login"));
    }

    #[test]
    fn transport_failures_are_classified_by_kind() {
        assert_eq!(
            classify_transport_error("operation timed out").code,
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
    fn a_body_without_triblers_error_envelope_is_kept_verbatim() {
        assert_eq!(error_detail("<html>nginx</html>"), "<html>nginx</html>");
        assert_eq!(
            error_detail(r#"{"error":{"handled":false,"message":"boom"}}"#),
            "boom"
        );
    }

    // -----------------------------------------------------------------------
    // Descriptor and warnings
    // -----------------------------------------------------------------------

    #[test]
    fn the_descriptor_reports_what_this_client_can_actually_do() {
        let raw = scryer_describe(String::new()).expect("describe");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("descriptor json");
        let capabilities = &value["provider"]["capabilities"];

        assert_eq!(capabilities["pause"], true);
        assert_eq!(capabilities["resume"], true);
        assert_eq!(capabilities["remove_with_data"], true);
        assert_eq!(capabilities["mark_imported"], false);
        assert_eq!(capabilities["mark_imported_non_destructive"], false);
        assert_eq!(capabilities["seed_limits"], false);
        assert_eq!(capabilities["queue_priority"], false);

        let inputs = value["provider"]["accepted_inputs"]
            .as_array()
            .expect("accepted inputs");
        assert!(inputs.iter().any(|kind| kind == "magnet_uri"));
        assert!(inputs.iter().any(|kind| kind == "torrent_bytes"));

        let url_base = value["provider"]["config_fields"]
            .as_array()
            .expect("config fields")
            .iter()
            .find(|field| field["key"] == "url_base")
            .expect("url_base field");
        assert_eq!(url_base["role"], "connection_url");
    }

    #[test]
    fn the_provider_warning_carries_the_tested_version_range() {
        let warnings = provider_warnings(Some("8.4.3"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("8.0.7"));
        assert!(warnings[0].contains("8.4.3"));

        let old = provider_warnings(Some("7.14.0"));
        assert_eq!(old.len(), 2);
        assert!(old[1].contains("predates the 8.x REST API"));

        assert_eq!(provider_warnings(None).len(), 1);
    }

    #[test]
    fn a_version_string_yields_its_major() {
        assert_eq!(major_version("8.4.3"), Some(8));
        assert_eq!(major_version("v7.14.0"), Some(7));
        assert_eq!(major_version("8.4.1-RC1"), Some(8));
        assert_eq!(major_version("git"), None);
    }

    // -----------------------------------------------------------------------
    // Misc
    // -----------------------------------------------------------------------

    #[test]
    fn query_parameters_are_percent_encoded() {
        assert_eq!(
            encode_query(&[
                ("destination".to_string(), "/data/TV Shows".to_string()),
                ("anon_hops".to_string(), "1".to_string()),
            ]),
            "destination=%2Fdata%2FTV%20Shows&anon_hops=1"
        );
    }

    #[test]
    fn an_info_hash_is_normalised_to_lowercase_hex() {
        assert_eq!(
            normalize_hash(" ABCDEF0123456789ABCDEF0123456789ABCDEF01 "),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn localhost_is_recognised_in_the_api_root() {
        assert!(is_localhost_url("http://localhost:20100/api"));
        assert!(is_localhost_url("http://127.0.0.1:20100/api"));
        assert!(is_localhost_url("http://[::1]:20100/api"));
        assert!(!is_localhost_url("http://tribler.lan:20100/api"));
    }

    #[test]
    fn the_user_agent_carries_the_crate_version() {
        assert_eq!(
            user_agent(),
            format!("scryer-tribler-plugin/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn history_is_empty_because_the_queue_already_carries_everything() {
        let raw = scryer_download_list_history(String::new()).expect("list history");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("history json");
        assert_eq!(value["ok"], serde_json::json!([]));
    }

    #[test]
    fn a_force_start_is_refused_as_unsupported() {
        let request = serde_json::json!({
            "action": "force_start",
            "client_item_id": "abcdef0123456789abcdef0123456789abcdef01",
        });
        let error = control(&request.to_string()).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Unsupported);
    }

    #[test]
    fn an_unreadable_request_is_permanent_not_temporary() {
        let error = parse_request::<PluginDownloadClientControlRequest>("not json").unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent);
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
