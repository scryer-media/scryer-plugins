use base64::{Engine as _, engine::general_purpose::STANDARD};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, DownloadClientCapabilities, DownloadClientDescriptor,
    DownloadControlAction, DownloadInputKind, DownloadIsolationMode, DownloadItemState,
    DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, PluginTorrentItem, ProviderDescriptor, SDK_VERSION,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};

const SID_VAR_KEY: &str = "downloadstation.sid";
const APIS_VAR_KEY: &str = "downloadstation.apis";
const DSM_VAR_KEY: &str = "downloadstation.dsm";
const SHARED_FOLDERS_VAR_KEY: &str = "downloadstation.shared_folders";

/// Sonarr caches `SYNO.API.Info` per host for an hour
/// (Proxies/DiskStationProxyBase.cs:263) and its task-proxy choice for ten
/// minutes (Proxies/DownloadStationTaskProxySelector.cs:48). One hour covers
/// both here, because the selection *is* the api-info document.
const APIS_TTL_SECONDS: i64 = 3_600;
/// Sonarr's `SerialNumberProvider` caches for five minutes
/// (SerialNumberProvider.cs:35).
const DSM_TTL_SECONDS: i64 = 300;
/// Sonarr's `SharedFolderResolver` caches for an hour
/// (SharedFolderResolver.cs:49).
const SHARED_FOLDER_TTL_SECONDS: i64 = 3_600;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A typed failure.
///
/// `Err(Error::msg(..))` out of a plugin function becomes
/// `PluginErrorCode::Temporary` in the PDK bridge, which would make a rejected
/// password look retryable. Every fallible path in this client therefore
/// carries its own code, mirroring the distinctions Sonarr draws in
/// `TestConnection`/`Test(failures)` and in `Responses/DiskStationError.cs`.
#[derive(Debug, Clone)]
struct DsError {
    code: PluginErrorCode,
    public_message: String,
    debug_message: Option<String>,
    api_code: Option<i64>,
}

impl DsError {
    fn new(code: PluginErrorCode, public_message: impl Into<String>) -> Self {
        Self {
            code,
            public_message: public_message.into(),
            debug_message: None,
            api_code: None,
        }
    }

    fn with_debug(mut self, debug_message: impl Into<String>) -> Self {
        self.debug_message = Some(debug_message.into());
        self
    }

    fn with_api_code(mut self, api_code: i64) -> Self {
        self.api_code = Some(api_code);
        self
    }

    /// DiskStation's session errors (`Responses/DiskStationError.cs:85`). The
    /// sid is cleared and the call retried once before the error escapes.
    fn is_session_error(&self) -> bool {
        matches!(self.api_code, Some(105 | 106 | 107 | 119))
    }

    fn into_plugin_error(self) -> PluginError {
        PluginError {
            code: self.code,
            public_message: self.public_message,
            debug_message: self.debug_message,
            retry_after_seconds: None,
            details: None,
        }
    }
}

fn respond<T: Serialize>(result: Result<T, DsError>) -> FnResult<String> {
    let payload = match result {
        Ok(value) => PluginResult::Ok(value),
        Err(error) => PluginResult::Err(error.into_plugin_error()),
    };
    Ok(serde_json::to_string(&payload)?)
}

// ---------------------------------------------------------------------------
// DiskStation APIs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DsApi {
    Info,
    Auth,
    DownloadStationInfo,
    DownloadStationTask,
    DownloadStation2Task,
    FileStationList,
    DsmInfo,
}

impl DsApi {
    fn name(self) -> &'static str {
        match self {
            DsApi::Info => "SYNO.API.Info",
            DsApi::Auth => "SYNO.API.Auth",
            DsApi::DownloadStationInfo => "SYNO.DownloadStation.Info",
            DsApi::DownloadStationTask => "SYNO.DownloadStation.Task",
            DsApi::DownloadStation2Task => "SYNO.DownloadStation2.Task",
            DsApi::FileStationList => "SYNO.FileStation.List",
            DsApi::DsmInfo => "SYNO.DSM.Info",
        }
    }
}

/// `Responses/DiskStationError.cs:14-25`.
fn common_error_message(code: i64) -> Option<&'static str> {
    Some(match code {
        100 => "Unknown error",
        101 => "Invalid parameter",
        102 => "The requested API does not exist",
        103 => "The requested method does not exist",
        104 => "The requested version does not support the functionality",
        105 => "The logged in session does not have permission",
        106 => "Session timeout",
        107 => "Session interrupted by duplicate login",
        119 => "SID not found",
        _ => return None,
    })
}

/// `Responses/DiskStationError.cs:27-39`.
fn auth_error_message(code: i64) -> Option<&'static str> {
    Some(match code {
        400 => "No such account or incorrect password",
        401 => "Disabled account",
        402 => "Denied permission",
        403 => "2-step authentication code required",
        404 => "Failed to authenticate 2-step authentication code",
        406 => "Enforce to authenticate with 2-factor authentication code",
        407 => "Blocked IP source",
        408 => "Expired password cannot change",
        409 => "Expired password",
        410 => "Password must be changed",
        _ => return None,
    })
}

/// `Responses/DiskStationError.cs:41-52`.
fn task_error_message(code: i64) -> Option<&'static str> {
    Some(match code {
        400 => "File upload failed",
        401 => "Max number of tasks reached",
        402 => "Destination denied",
        403 => "Destination does not exist",
        404 => "Invalid task id",
        405 => "Invalid task action",
        406 => "No default destination",
        407 => "Set destination failed",
        408 => "File does not exist",
        _ => return None,
    })
}

/// `Responses/DiskStationError.cs:54-80`.
fn file_station_error_message(code: i64) -> Option<&'static str> {
    Some(match code {
        160 => "Permission denied. Give your user access to FileStation.",
        400 => "Invalid parameter of file operation",
        401 => "Unknown error of file operation",
        402 => "System is too busy",
        403 => "Invalid user does this file operation",
        404 => "Invalid group does this file operation",
        405 => "Invalid user and group does this file operation",
        406 => "Can't get user/group information from the account server",
        407 => "Operation not permitted",
        408 => "No such file or directory",
        409 => "Non-supported file system",
        410 => "Failed to connect internet-based file system (ex: CIFS)",
        411 => "Read-only file system",
        412 => "Filename too long in the non-encrypted file system",
        413 => "Filename too long in the encrypted file system",
        414 => "File already exists",
        415 => "Disk quota exceeded",
        416 => "No space left on device",
        417 => "Input/output error",
        418 => "Illegal name or path",
        419 => "Illegal file name",
        420 => "Illegal file name on FAT file system",
        421 => "Device or resource busy",
        599 => "No such task of the file operation",
        _ => return None,
    })
}

/// `Responses/DiskStationError.cs:87-110` — the same lookup order Sonarr uses.
fn api_error_message(api: DsApi, code: i64) -> String {
    let table = match api {
        DsApi::Auth => auth_error_message(code),
        DsApi::DownloadStationTask | DsApi::DownloadStation2Task => task_error_message(code),
        DsApi::FileStationList => file_station_error_message(code),
        _ => None,
    };
    table
        .or_else(|| common_error_message(code))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{code} - Unknown error"))
}

/// Scryer's typed classification of a DiskStation error code.
///
/// Sonarr expresses the same distinctions through exception classes: code 105
/// becomes a `DownloadClientAuthenticationException`
/// (Proxies/DiskStationProxyBase.cs:116-119), the other session codes only
/// evict the cached session, and everything else is a `DownloadClientException`
/// whose text comes from the tables above.
fn api_error_code(api: DsApi, code: i64) -> PluginErrorCode {
    match (api, code) {
        (_, 105) => PluginErrorCode::AuthFailed,
        // Session timeout / duplicate login / missing sid. The call has already
        // been retried once with a fresh session by the time this classifies.
        (_, 106 | 107 | 119) => PluginErrorCode::Temporary,
        (DsApi::Auth, 400..=410) => PluginErrorCode::AuthFailed,
        // Destination denied / does not exist / no default destination / set
        // destination failed — all of them are the operator's Download Station
        // configuration, not a transient fault.
        (DsApi::DownloadStationTask | DsApi::DownloadStation2Task, 402 | 403 | 406 | 407) => {
            PluginErrorCode::InvalidConfig
        }
        (DsApi::FileStationList, 160 | 408) => PluginErrorCode::InvalidConfig,
        // "API/method/version does not exist" is a capability answer, not a
        // fault: this DSM cannot do what was asked.
        (_, 102..=104) => PluginErrorCode::Unsupported,
        (_, 101) => PluginErrorCode::Permanent,
        _ => PluginErrorCode::Temporary,
    }
}

fn disk_station_error(api: DsApi, code: i64) -> DsError {
    DsError::new(
        api_error_code(api, code),
        format!("Download Station: {}", api_error_message(api, code)),
    )
    .with_debug(format!("{} returned error code {code}", api.name()))
    .with_api_code(code)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DsConfig {
    base_url: String,
    host: String,
    username: String,
    password: String,
    category: String,
    directory: String,
}

impl DsConfig {
    fn load() -> Self {
        let host = config_value("host").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = config_value("port").unwrap_or_else(|| "5000".to_string());
        let scheme = if config_bool("use_ssl", false) {
            "https"
        } else {
            "http"
        };
        Self {
            base_url: format!("{scheme}://{host}:{port}"),
            host,
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            category: config_value("category").unwrap_or_default(),
            directory: config_value("directory").unwrap_or_default(),
        }
    }
}

/// `DownloadStationSettingsValidator` (DownloadStationSettings.cs:11-25).
///
/// Sonarr enforces these when the settings are saved; Scryer's equivalent
/// surface is `test_connection`.
fn validate_settings(config: &DsConfig) -> Result<(), DsError> {
    if config.directory.starts_with('/') {
        return Err(DsError::new(
            PluginErrorCode::InvalidConfig,
            "Directory cannot start with /: Download Station destinations are relative to a shared folder",
        )
        .with_debug(format!("directory={}", config.directory)));
    }
    if !category_is_valid(&config.category) {
        return Err(DsError::new(
            PluginErrorCode::InvalidConfig,
            "Category allows only the characters a-z and -, with an optional leading dot",
        )
        .with_debug(format!("category={}", config.category)));
    }
    validate_scope_exclusivity(config)
}

/// Sonarr: `RuleFor(c => c.TvCategory).Empty().When(TvDirectory is set)`
/// (DownloadStationSettings.cs:22-24).
fn validate_scope_exclusivity(config: &DsConfig) -> Result<(), DsError> {
    if !config.directory.trim().is_empty() && !config.category.trim().is_empty() {
        return Err(DsError::new(
            PluginErrorCode::InvalidConfig,
            "Cannot use Category and Directory together: clear one of them",
        )
        .with_debug(format!(
            "category={} directory={}",
            config.category, config.directory
        )));
    }
    Ok(())
}

/// Sonarr's `^\.?[-a-z]*$` with `RegexOptions.IgnoreCase`.
fn category_is_valid(category: &str) -> bool {
    let rest = category.strip_prefix('.').unwrap_or(category);
    rest.chars().all(|ch| ch.is_ascii_alphabetic() || ch == '-')
}

// ---------------------------------------------------------------------------
// API discovery and session
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApiSelection {
    auth: ApiInfo,
    task: ApiInfo,
    task_v2: bool,
    info: Option<ApiInfo>,
    dsm_info: Option<ApiInfo>,
    file_station: Option<ApiInfo>,
}

impl ApiSelection {
    fn task_api(&self) -> DsApi {
        if self.task_v2 {
            DsApi::DownloadStation2Task
        } else {
            DsApi::DownloadStationTask
        }
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct ApiInfo {
    #[serde(default, alias = "maxVersion")]
    max_version: i64,
    #[serde(default, alias = "minVersion")]
    min_version: i64,
    #[serde(default)]
    path: String,
}

#[derive(Serialize, Deserialize)]
struct CachedApis {
    endpoint: String,
    expires_at: i64,
    apis: ApiSelection,
}

#[derive(Deserialize)]
struct DiskResponse<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    error: Option<DiskError>,
    data: T,
}

#[derive(Default, Deserialize)]
struct DiskError {
    #[serde(default)]
    code: i64,
}

#[derive(Default, Deserialize)]
struct AuthData {
    #[serde(default, rename = "sid", alias = "SId")]
    sid: String,
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct TaskListV1 {
    #[serde(default)]
    tasks: Vec<DsTask>,
}

#[derive(Default, Deserialize)]
struct TaskListV2 {
    #[serde(default)]
    task: Vec<DsTaskV2>,
    #[serde(default)]
    total: i64,
}

#[derive(Default, Deserialize, Clone)]
struct DsTaskV2 {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "lenient_i64")]
    size: i64,
    #[serde(default, rename = "type")]
    task_type: String,
    #[serde(default, deserialize_with = "lenient_i64")]
    status: i64,
    #[serde(default)]
    additional: DsAdditional,
}

#[derive(Default, Deserialize, Clone)]
struct DsTask {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "lenient_i64")]
    size: i64,
    #[serde(default, rename = "type")]
    task_type: String,
    #[serde(default, deserialize_with = "lenient_status")]
    status: DsStatus,
    #[serde(default, rename = "status_extra")]
    status_extra: HashMap<String, serde_json::Value>,
    #[serde(default)]
    additional: DsAdditional,
}

/// Download Station is not consistent about the JSON type of the values inside
/// `additional`: `size_downloaded` is a string on some DSM builds and a number
/// on others, `completed_time` is a number, `priority` a string. Sonarr models
/// these as `Dictionary<string, string>` and lets Newtonsoft coerce; a Rust
/// `HashMap<String, String>` would instead fail to deserialize the **whole
/// task list**, so every queue poll on such a DSM would error out. Keep the raw
/// values and coerce at the point of use.
#[derive(Default, Deserialize, Clone)]
struct DsAdditional {
    #[serde(default)]
    detail: HashMap<String, serde_json::Value>,
    #[serde(default)]
    transfer: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
enum DsStatus {
    #[default]
    Unknown,
    Waiting,
    Downloading,
    Paused,
    Finishing,
    Finished,
    HashChecking,
    Seeding,
    FilehostingWaiting,
    Extracting,
    Error,
    CaptchaNeeded,
}

fn status_from_name(value: &str) -> DsStatus {
    match value.trim().to_ascii_lowercase().as_str() {
        "waiting" => DsStatus::Waiting,
        "downloading" => DsStatus::Downloading,
        "paused" => DsStatus::Paused,
        "finishing" => DsStatus::Finishing,
        "finished" => DsStatus::Finished,
        "hash_checking" => DsStatus::HashChecking,
        "seeding" => DsStatus::Seeding,
        "filehosting_waiting" => DsStatus::FilehostingWaiting,
        "extracting" => DsStatus::Extracting,
        "error" => DsStatus::Error,
        "captcha_needed" => DsStatus::CaptchaNeeded,
        // Sonarr's `UnderscoreStringEnumConverter` is constructed with
        // `DownloadStationTaskStatus.Unknown` as its fallback
        // (DownloadStationTask.cs:25), pinned by
        // `DownloadStationsTaskStatusJsonConverterFixture.should_return_unknown_if_unknown_enum_value`.
        // A strict serde enum would fail the whole list instead, and an
        // unrecognised state must keep the download polling (`GetStatus` maps
        // Unknown to Queued/Completed), never fail it.
        _ => DsStatus::Unknown,
    }
}

fn status_from_int(value: i64) -> DsStatus {
    match value {
        1 => DsStatus::Waiting,
        2 => DsStatus::Downloading,
        3 => DsStatus::Paused,
        4 => DsStatus::Finishing,
        5 => DsStatus::Finished,
        6 => DsStatus::HashChecking,
        7 => DsStatus::Seeding,
        8 => DsStatus::FilehostingWaiting,
        9 => DsStatus::Extracting,
        10 => DsStatus::Error,
        11 => DsStatus::CaptchaNeeded,
        _ => DsStatus::Unknown,
    }
}

fn status_name(status: DsStatus) -> &'static str {
    match status {
        DsStatus::Unknown => "unknown",
        DsStatus::Waiting => "waiting",
        DsStatus::Downloading => "downloading",
        DsStatus::Paused => "paused",
        DsStatus::Finishing => "finishing",
        DsStatus::Finished => "finished",
        DsStatus::HashChecking => "hash_checking",
        DsStatus::Seeding => "seeding",
        DsStatus::FilehostingWaiting => "filehosting_waiting",
        DsStatus::Extracting => "extracting",
        DsStatus::Error => "error",
        DsStatus::CaptchaNeeded => "captcha_needed",
    }
}

fn lenient_status<'de, D>(deserializer: D) -> Result<DsStatus, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match &value {
        serde_json::Value::String(text) => status_from_name(text),
        serde_json::Value::Number(number) => number
            .as_i64()
            .map(status_from_int)
            .unwrap_or(DsStatus::Unknown),
        _ => DsStatus::Unknown,
    })
}

fn lenient_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value_to_i64(&value).unwrap_or_default())
}

fn value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn value_to_i64(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64)),
        serde_json::Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

impl DsTask {
    fn detail_string(&self, key: &str) -> Option<String> {
        self.additional.detail.get(key).and_then(value_to_string)
    }

    fn detail_i64(&self, key: &str) -> Option<i64> {
        self.additional.detail.get(key).and_then(value_to_i64)
    }

    fn transfer_i64(&self, key: &str) -> Option<i64> {
        self.additional.transfer.get(key).and_then(value_to_i64)
    }

    fn status_extra_string(&self, key: &str) -> Option<String> {
        self.status_extra.get(key).and_then(value_to_string)
    }

    /// Download Station reports the destination without a leading slash
    /// (`shared/folder`); the leading slash is what makes it resolvable through
    /// FileStation, exactly as Sonarr notes in `GetStatus`.
    fn destination(&self) -> String {
        self.detail_string("destination").unwrap_or_default()
    }

    fn is_bt(&self) -> bool {
        self.task_type.eq_ignore_ascii_case("bt")
    }

    fn is_nzb(&self) -> bool {
        self.task_type.eq_ignore_ascii_case("nzb")
    }
}

fn is_supported_task_type(task: &DsTask) -> bool {
    task.is_bt() || task.is_nzb()
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "downloadstation".to_string(),
        name: "Download Station".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "downloadstation".to_string(),
            provider_aliases: vec!["synology-download-station".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentFile,
                DownloadInputKind::Nzb,
                DownloadInputKind::NzbUrl,
            ],
            // Download Station has no tag or label concept at all: the only
            // isolation it offers is where the task writes. `category` is
            // Sonarr's `TvCategory`, a sub-folder of the default destination
            // (TorrentDownloadStation.cs:458-461), and `directory` /
            // `routing.download_directory` are the destination itself.
            isolation_modes: vec![
                DownloadIsolationMode::Category,
                DownloadIsolationMode::Directory,
            ],
            capabilities: DownloadClientCapabilities {
                pause: false,
                resume: false,
                remove: true,
                remove_with_data: false,
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
                        DownloadIsolationMode::Directory,
                    ],
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
                    ..DownloadTorrentCapabilities::default()
                }),
                // SDK 3.10 addition. `false` is the SDK's own default and therefore exactly
                // what this client's pre-3.10 descriptor already meant to a 3.10 host;
                // advertising category-scoped feedback would be a behaviour change, not a
                // transport one, so it stays off across the component migration.
                category_scoped_feedback: false,
                // Download Station exposes no per-task label, tag or category
                // to write back to, so there is nothing a post-import mark
                // could apply. The bridge maps a missing
                // `mark_imported_non_destructive` to a silent `Ok(())`, which
                // is the truthful answer here, and the descriptor says so
                // rather than advertising a handoff that would be a no-op.
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
            Some("127.0.0.1"),
            None,
        ),
        field(
            "port",
            "Port",
            ConfigFieldType::Number,
            true,
            Some("5000"),
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
            "category",
            "Category",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Sub-folder of the Download Station default destination to use, \
                 and the folder Scryer treats as its own. Letters and - only. \
                 Cannot be combined with Directory.",
            ),
        ),
        field(
            "directory",
            "Directory",
            ConfigFieldType::Path,
            false,
            None,
            Some(
                "Optional shared folder to put downloads into, relative to a \
                 Download Station shared folder and without a leading slash. \
                 Leave blank to use the default Download Station location.",
            ),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

pub fn scryer_download_add(input: String) -> FnResult<String> {
    respond(add_download(&input))
}

fn add_download(input: &str) -> Result<PluginDownloadClientAddResponse, DsError> {
    let request: PluginDownloadClientAddRequest = serde_json::from_str(input).map_err(|error| {
        DsError::new(PluginErrorCode::Permanent, "malformed download add request")
            .with_debug(error.to_string())
    })?;
    let config = DsConfig::load();
    // Category and directory together is rejected by `test_connection`, not
    // here: `download_directory` and `matches_scope` both let the directory
    // win, so the combination cannot misroute a grab, and refusing adds for a
    // configuration that worked before would fail grabs on upgrade.
    let apis = select_apis(&config)?;
    // Sonarr resolves the hashed serial number *before* it creates the task
    // (TorrentDownloadStation.cs:168-170), pinned by
    // `Download_should_throw_and_not_add_task_if_cannot_get_serial_number`: a
    // task Scryer cannot address is worse than a task that was never created.
    let serial = dsm_info(&config, &apis)?.serial_hash;
    let destination = add_destination(&config, &apis, &request)?;

    // Snapshot before the add so a pre-existing task with the same uri cannot
    // be mistaken for the one just created.
    let before = list_tasks(&config, &apis)?
        .into_iter()
        .map(|task| task.id)
        .collect::<HashSet<_>>();

    let expected = submit_task(&config, &apis, &request, destination.as_deref())?;
    let tasks = list_tasks(&config, &apis)?;
    let id = identify_added_task(&tasks, &expected, &before)?;

    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty());
    Ok(PluginDownloadClientAddResponse {
        client_item_id: format!("{serial}:{id}"),
        info_hash: hash,
    })
}

/// What the created task's `Additional.Detail["uri"]` is expected to be, and
/// which task type it will have.
struct ExpectedTask {
    uris: Vec<String>,
    is_nzb: bool,
}

fn submit_task(
    config: &DsConfig,
    apis: &ApiSelection,
    request: &PluginDownloadClientAddRequest,
    destination: Option<&str>,
) -> Result<ExpectedTask, DsError> {
    let source = &request.source;
    if let Some(bytes) = source.torrent_bytes_base64.as_deref() {
        let file_name = upload_file_name(request, ".torrent");
        add_file(
            config,
            apis,
            FileUpload {
                file_name: &file_name,
                bytes_base64: bytes,
                destination,
                content_type: source
                    .torrent_content_type
                    .as_deref()
                    .unwrap_or("application/x-bittorrent"),
                payload_label: "torrent_bytes_base64",
            },
        )?;
        // Sonarr matches a torrent upload on the file name **without** its
        // extension (TorrentDownloadStation.cs:191).
        return Ok(ExpectedTask {
            uris: vec![strip_extension(&file_name), file_name],
            is_nzb: false,
        });
    }
    if let Some(bytes) = source.nzb_bytes_base64.as_deref() {
        let file_name = upload_file_name(request, ".nzb");
        add_file(
            config,
            apis,
            FileUpload {
                file_name: &file_name,
                bytes_base64: bytes,
                destination,
                content_type: source
                    .nzb_content_type
                    .as_deref()
                    .unwrap_or("application/x-nzb"),
                payload_label: "nzb_bytes_base64",
            },
        )?;
        // Sonarr matches an NZB upload on the file name **with** its extension
        // (UsenetDownloadStation.cs:183).
        return Ok(ExpectedTask {
            uris: vec![file_name.clone(), strip_extension(&file_name)],
            is_nzb: true,
        });
    }
    let Some(url) = source_url(request) else {
        return Err(DsError::new(
            PluginErrorCode::Permanent,
            "download source is missing",
        ));
    };
    add_url(config, apis, &url, destination)?;
    Ok(ExpectedTask {
        uris: vec![url],
        is_nzb: matches!(
            source.kind,
            DownloadInputKind::Nzb | DownloadInputKind::NzbUrl
        ),
    })
}

/// Sonarr identifies the created task with `SingleOrDefault` on the uri
/// (TorrentDownloadStation.cs:172-183, UsenetDownloadStation.cs:183-195), which
/// hands back a *pre-existing* task when one already carries that uri. The
/// before/after snapshot narrows that to the task this call created; the plain
/// uri match remains as the fallback, because Download Station legitimately
/// de-duplicates an identical magnet into the existing task.
fn identify_added_task(
    tasks: &[DsTask],
    expected: &ExpectedTask,
    before: &HashSet<String>,
) -> Result<String, DsError> {
    let matching = tasks
        .iter()
        .filter(|task| {
            task.detail_string("uri")
                .is_some_and(|uri| expected.uris.contains(&uri))
        })
        .collect::<Vec<_>>();
    let fresh_matching = matching
        .iter()
        .filter(|task| !before.contains(&task.id))
        .collect::<Vec<_>>();
    match fresh_matching.len() {
        1 => return Ok(fresh_matching[0].id.clone()),
        0 => {}
        _ => return Err(ambiguous_task_error(fresh_matching.len())),
    }

    // The uri Download Station stored can differ from what was submitted (a
    // normalised magnet, a renamed upload). A single task that did not exist
    // before this call is still unambiguous evidence.
    let fresh = tasks
        .iter()
        .filter(|task| !before.contains(&task.id) && task.is_nzb() == expected.is_nzb)
        .collect::<Vec<_>>();
    match fresh.len() {
        1 => return Ok(fresh[0].id.clone()),
        0 => {}
        _ if matching.is_empty() => return Err(ambiguous_task_error(fresh.len())),
        _ => {}
    }

    match matching.len() {
        1 => Ok(matching[0].id.clone()),
        0 => Err(DsError::new(
            PluginErrorCode::Temporary,
            "Download Station did not return the added task",
        )
        .with_debug(format!("expected uri one of {:?}", expected.uris))),
        count => Err(ambiguous_task_error(count)),
    }
}

fn ambiguous_task_error(count: usize) -> DsError {
    DsError::new(
        PluginErrorCode::Permanent,
        "Download Station returned more than one task for this release; Scryer cannot tell which one it created",
    )
    .with_debug(format!("{count} candidate tasks"))
}

/// Where the task must be created.
///
/// `routing.download_directory` is the core's per-download routing and wins
/// (the descriptor advertises `per_download_directory`); otherwise this is
/// Sonarr's `GetDownloadDirectory` (TorrentDownloadStation.cs:449-464).
/// `None` means "let Download Station use its own default", which is what
/// Sonarr does when the destination is empty (Proxies/DownloadStationTaskProxyV1.cs:43-46).
fn add_destination(
    config: &DsConfig,
    apis: &ApiSelection,
    request: &PluginDownloadClientAddRequest,
) -> Result<Option<String>, DsError> {
    if let Some(routed) = request
        .routing
        .download_directory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        // Download Station destinations are shared-folder relative; Sonarr's
        // own settings validator forbids a leading slash
        // (DownloadStationSettings.cs:16-18) and its reader trims one anyway.
        let routed = routed.trim_start_matches('/').trim_end_matches('/');
        ensure_routed_directory_is_in_scope(config, routed)?;
        return Ok(Some(routed.to_string()));
    }
    download_directory(config, apis)
}

/// A routed directory outside this client's own scope would be created and then
/// never seen again, because the queue poll filters on the configured
/// directory or category. Sonarr has no per-download directory and therefore no
/// equivalent; refusing the add is far better than losing the grab.
fn ensure_routed_directory_is_in_scope(config: &DsConfig, routed: &str) -> Result<(), DsError> {
    if !config.directory.trim().is_empty() {
        if path_is_within(&config.directory, routed) {
            return Ok(());
        }
        return Err(DsError::new(
            PluginErrorCode::InvalidConfig,
            format!(
                "Routed download directory '{routed}' is outside the configured Download Station directory '{}', so Scryer would never see the task again",
                config.directory
            ),
        ));
    }
    if !config.category.trim().is_empty() && category_component(routed, &config.category).is_none()
    {
        return Err(DsError::new(
            PluginErrorCode::InvalidConfig,
            format!(
                "Routed download directory '{routed}' does not contain the configured Download Station category '{}', so Scryer would never see the task again",
                config.category
            ),
        ));
    }
    Ok(())
}

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    respond(list_queue())
}

fn list_queue() -> Result<Vec<PluginDownloadItem>, DsError> {
    let config = DsConfig::load();
    let apis = select_apis(&config)?;
    let dsm = dsm_info(&config, &apis)?;
    let tasks = list_tasks(&config, &apis)?;
    let mut cache = SharedFolderCache::load(&config);
    let items = {
        let mut resolve =
            |path: &str| remap_to_full_path(&config, &apis, &dsm.serial_hash, &mut cache, path);
        build_items(&config, &dsm.serial_hash, tasks, &mut resolve)
    };
    cache.save(&config);
    items
}

/// Download Station keeps no separate failed history: a failed task stays in
/// the same task list the queue poll already reads, and the PDK bridge merges
/// `list_queue` with the terminal rows of `list_history`. Repeating the (v2:
/// two-request) listing here would only double every poll.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    respond(Ok::<Vec<PluginDownloadItem>, DsError>(Vec::new()))
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    respond(list_completed())
}

fn list_completed() -> Result<Vec<PluginCompletedDownload>, DsError> {
    let config = DsConfig::load();
    let apis = select_apis(&config)?;
    let dsm = dsm_info(&config, &apis)?;
    let tasks = list_tasks(&config, &apis)?;
    let mut cache = SharedFolderCache::load(&config);
    let downloads = {
        let mut resolve =
            |path: &str| remap_to_full_path(&config, &apis, &dsm.serial_hash, &mut cache, path);
        build_completed(&config, &dsm.serial_hash, tasks, &mut resolve)
    };
    cache.save(&config);
    downloads
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    respond(control(&input))
}

fn control(input: &str) -> Result<(), DsError> {
    let request: PluginDownloadClientControlRequest =
        serde_json::from_str(input).map_err(|error| {
            DsError::new(PluginErrorCode::Permanent, "malformed control request")
                .with_debug(error.to_string())
        })?;
    match request.action {
        DownloadControlAction::Remove => {
            if request.remove_data {
                return Err(DsError::new(
                    PluginErrorCode::Unsupported,
                    "Download Station's delete API has no delete-data flag; Scryer removes the payload itself",
                ));
            }
            let config = DsConfig::load();
            let apis = select_apis(&config)?;
            let id = parse_download_id(&request.client_item_id);
            let _: serde_json::Value = api_call(
                &config,
                &apis,
                ApiCall {
                    api: apis.task_api(),
                    info: &apis.task,
                    method: "delete",
                    version: if apis.task_v2 { 2 } else { 1 },
                    params: vec![
                        ("id".to_string(), id),
                        ("force_complete".to_string(), "false".to_string()),
                    ],
                    auth: true,
                },
            )?;
            Ok(())
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => Err(DsError::new(
            PluginErrorCode::Unsupported,
            "Download Station control action is not implemented by Scryer's Download Station client",
        )),
    }
}

/// Download Station has no per-task label, tag or category to write back to, so
/// there is no post-import mark to apply and this is a documented no-op.
///
/// In particular `request.post_import_isolation` is **not** a destination: the
/// core builds it from the download's own grab category, replicated across the
/// isolation modes, so it describes what the download was grabbed under rather
/// than where it should move. Nothing here re-applies it.
///
/// Removal of a finished task is the core's decision through the seeding gate,
/// never this plugin's.
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let _request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    respond(Ok::<(), DsError>(()))
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    respond(client_status())
}

fn client_status() -> Result<PluginDownloadClientStatus, DsError> {
    let config = DsConfig::load();
    let apis = select_apis(&config)?;
    let dsm = dsm_info(&config, &apis)?;
    let mut cache = SharedFolderCache::load(&config);
    let roots = match download_directory(&config, &apis)? {
        Some(directory) => {
            // Download Station returns the path without the leading `/`, but the
            // leading slash is what makes it resolvable through FileStation —
            // the same note Sonarr leaves in `GetStatus`.
            let resolved = remap_to_full_path(
                &config,
                &apis,
                &dsm.serial_hash,
                &mut cache,
                &format!("/{directory}"),
            )?;
            vec![resolved]
        }
        None => Vec::new(),
    };
    cache.save(&config);
    Ok(PluginDownloadClientStatus {
        version: dsm.version.clone(),
        is_localhost: Some(matches!(config.host.as_str(), "127.0.0.1" | "localhost")),
        remote_output_roots: roots,
        removes_completed_downloads: Some(false),
        sorting_mode: Some(
            if apis.task_v2 {
                "downloadstation-v2"
            } else {
                "downloadstation-v1"
            }
            .to_string(),
        ),
        warnings: vec![
            // Sonarr's `DownloadClientDownloadStationProviderMessage`
            // (Localization/Core/en.json:466).
            "Scryer is unable to connect to Download Station if 2-Factor Authentication is enabled on your DSM account".to_string(),
            "Remove with data is unavailable: Download Station's delete API has no delete-data flag".to_string(),
        ],
    })
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    respond(test_connection())
}

/// Sonarr's `Test(failures)` = `TestConnection()` + `TestOutputPath()` +
/// `TestGetTorrents()` (TorrentDownloadStation.cs:206-216), with the settings
/// validator in front of it.
fn test_connection() -> Result<String, DsError> {
    let config = DsConfig::load();
    validate_settings(&config)?;

    // A test must not pass on a stale session or a stale api-info document.
    let _ = var::remove(SID_VAR_KEY);
    let _ = var::remove(APIS_VAR_KEY);
    let apis = select_apis(&config)?;
    authenticate(&config, &apis.auth, true)?;
    validate_task_api_version(&apis)?;
    test_output_path(&config, &apis)?;

    // TestGetTorrents / TestGetNZB: the full listing, including the shared
    // folder resolution the output paths depend on.
    let _ = list_queue()?;
    Ok("ok".to_string())
}

/// Sonarr's `ValidateVersion` (TorrentDownloadStation.cs:398-415).
fn validate_task_api_version(apis: &ApiSelection) -> Result<(), DsError> {
    if apis.task.min_version > 2 || apis.task.max_version < 2 {
        return Err(DsError::new(
            PluginErrorCode::Unsupported,
            format!(
                "Download Station API version not supported, should be at least 2. It supports from {} to {}",
                apis.task.min_version, apis.task.max_version
            ),
        )
        .with_debug(format!("{} version range", apis.task_api().name())));
    }
    Ok(())
}

/// Sonarr's `TestOutputPath` (TorrentDownloadStation.cs:309-357).
fn test_output_path(config: &DsConfig, apis: &ApiSelection) -> Result<(), DsError> {
    let Some(directory) = download_directory(config, apis)? else {
        return Err(DsError::new(
            PluginErrorCode::InvalidConfig,
            format!(
                "No default destination. You must login into your Diskstation as {} and manually set it up into DownloadStation settings under BT/HTTP/FTP/NZB -> Location.",
                config.username
            ),
        ));
    };
    let shared_folder = path_fragments(&directory)
        .first()
        .map(|fragment| (*fragment).to_string())
        .unwrap_or_default();
    let info = file_station_info(config, apis, &format!("/{directory}"))?;
    if info.additional.is_none() {
        return Err(DsError::new(
            PluginErrorCode::InvalidConfig,
            format!(
                "Shared folder does not exist. The Diskstation does not have a Shared Folder with the name '{shared_folder}', are you sure you specified it correctly?"
            ),
        ));
    }
    if !info.is_dir {
        return Err(DsError::new(
            PluginErrorCode::InvalidConfig,
            format!(
                "Folder does not exist. The folder '{directory}' does not exist, it must be created manually inside the Shared Folder '{shared_folder}'."
            ),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

fn select_apis(config: &DsConfig) -> Result<ApiSelection, DsError> {
    if let Some(cached) = cached_apis(config) {
        return Ok(cached);
    }
    let data: HashMap<String, ApiInfo> = api_info_query(
        config,
        "SYNO.API.Auth,SYNO.DownloadStation2.Task,SYNO.DownloadStation.Task,SYNO.DownloadStation.Info,SYNO.DSM.Info,SYNO.FileStation.List",
    )?;
    let auth = data.get("SYNO.API.Auth").cloned().ok_or_else(|| {
        DsError::new(
            PluginErrorCode::InvalidConfig,
            "This host did not advertise SYNO.API.Auth; check the address, port and SSL setting",
        )
    })?;
    let info = data.get("SYNO.DownloadStation.Info").cloned();
    let dsm_info = data.get("SYNO.DSM.Info").cloned();
    let file_station = data.get("SYNO.FileStation.List").cloned();
    let apis = match data.get("SYNO.DownloadStation2.Task").cloned() {
        // `DownloadStationTaskProxySelector.FetchProxy` prefers v2 and falls
        // back to v1 (Proxies/DownloadStationTaskProxySelector.cs:51-66).
        Some(task) => ApiSelection {
            auth,
            task,
            task_v2: true,
            info,
            dsm_info,
            file_station,
        },
        None => {
            let task = data.get("SYNO.DownloadStation.Task").cloned().ok_or_else(|| {
                DsError::new(
                    PluginErrorCode::Unsupported,
                    "Unable to determine Download Station's Task API version: neither SYNO.DownloadStation2.Task nor SYNO.DownloadStation.Task is available",
                )
            })?;
            ApiSelection {
                auth,
                task,
                task_v2: false,
                info,
                dsm_info,
                file_station,
            }
        }
    };
    store_apis(config, &apis);
    Ok(apis)
}

fn cached_apis(config: &DsConfig) -> Option<ApiSelection> {
    let cached: CachedApis = var::get(APIS_VAR_KEY).ok().flatten()?;
    (cached.endpoint == config.base_url && cached.expires_at > now_unix_seconds())
        .then_some(cached.apis)
}

fn store_apis(config: &DsConfig, apis: &ApiSelection) {
    let _ = var::set(
        APIS_VAR_KEY,
        CachedApis {
            endpoint: config.base_url.clone(),
            expires_at: now_unix_seconds() + APIS_TTL_SECONDS,
            apis: apis.clone(),
        },
    );
}

fn api_info_query(config: &DsConfig, query: &str) -> Result<HashMap<String, ApiInfo>, DsError> {
    let url = format!(
        "{}/webapi/query.cgi?api=SYNO.API.Info&version=1&method=query&query={}",
        config.base_url,
        urlencoding::encode(query)
    );
    request_disk_json(DsApi::Info, "GET", &url, None)
}

fn authenticate(config: &DsConfig, auth: &ApiInfo, force: bool) -> Result<String, DsError> {
    if !force
        && let Some(sid) = var::get::<String>(SID_VAR_KEY)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
    {
        return Ok(sid);
    }
    // Proxies/DiskStationProxyBase.cs:135.
    let version = if auth.max_version >= 7 { 6 } else { 2 };
    let query = vec![
        ("api".to_string(), DsApi::Auth.name().to_string()),
        ("version".to_string(), version.to_string()),
        ("method".to_string(), "login".to_string()),
        ("account".to_string(), config.username.clone()),
        ("passwd".to_string(), config.password.clone()),
        ("format".to_string(), "sid".to_string()),
        ("session".to_string(), "DownloadStation".to_string()),
    ];
    let url = format!(
        "{}/webapi/{}?{}",
        config.base_url,
        auth.path.trim_start_matches('/'),
        encode_query(&query)
    );
    let data: AuthData = request_disk_json(DsApi::Auth, "GET", &url, None)?;
    if data.sid.is_empty() {
        return Err(DsError::new(
            PluginErrorCode::AuthFailed,
            "Download Station did not return a session id",
        ));
    }
    let _ = var::set(SID_VAR_KEY, data.sid.clone());
    Ok(data.sid)
}

struct ApiCall<'a> {
    api: DsApi,
    info: &'a ApiInfo,
    method: &'a str,
    version: i64,
    params: Vec<(String, String)>,
    auth: bool,
}

/// One authenticated DiskStation call.
///
/// The api-info document is threaded in rather than re-queried: the previous
/// shape re-ran `query.cgi` for every authenticated request, roughly doubling
/// the cost of a queue poll. A session error clears the sid and retries once,
/// which is what Sonarr's session cache eviction achieves on the *next* call
/// (Proxies/DiskStationProxyBase.cs:112-120).
fn api_call<T: DeserializeOwned>(
    config: &DsConfig,
    apis: &ApiSelection,
    call: ApiCall<'_>,
) -> Result<T, DsError> {
    let mut force_login = false;
    loop {
        let mut query = vec![
            ("api".to_string(), call.api.name().to_string()),
            ("version".to_string(), call.version.to_string()),
            ("method".to_string(), call.method.to_string()),
        ];
        if call.auth {
            query.push((
                "_sid".to_string(),
                authenticate(config, &apis.auth, force_login)?,
            ));
        }
        query.extend_from_slice(&call.params);
        let url = format!(
            "{}/webapi/{}?{}",
            config.base_url,
            call.info.path.trim_start_matches('/'),
            encode_query(&query)
        );
        match request_disk_json(call.api, "GET", &url, None) {
            Err(error) if call.auth && error.is_session_error() && !force_login => {
                let _ = var::remove(SID_VAR_KEY);
                force_login = true;
            }
            other => return other,
        }
    }
}

fn request_disk_json<T: DeserializeOwned>(
    api: DsApi,
    method: &str,
    url: &str,
    body: Option<Vec<u8>>,
) -> Result<T, DsError> {
    let request = HttpRequest::new(url)
        .with_method(method)
        .with_header("User-Agent", user_agent());
    let response = http::request::<Vec<u8>>(&request, body).map_err(|error| {
        DsError::new(
            PluginErrorCode::UpstreamUnavailable,
            "Unable to connect to Diskstation, please check your settings",
        )
        .with_debug(error.to_string())
    })?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 300 {
        return Err(http_status_error(status, &text));
    }
    parse_disk_response(api, &text)
}

fn parse_disk_response<T: DeserializeOwned>(api: DsApi, text: &str) -> Result<T, DsError> {
    let parsed: DiskResponse<T> = serde_json::from_str(text).map_err(|error| {
        if text.trim_start().starts_with('<') {
            DsError::new(
                PluginErrorCode::InvalidConfig,
                "This host answered with a web page instead of the DiskStation API; check the address, port and SSL setting",
            )
            .with_debug(format!("{error}: {}", truncate(text, 300)))
        } else {
            DsError::new(
                PluginErrorCode::Temporary,
                "Download Station returned a response Scryer could not parse",
            )
            .with_debug(format!("{error}: {}", truncate(text, 300)))
        }
    })?;
    if parsed.success {
        return Ok(parsed.data);
    }
    let code = parsed.error.map(|error| error.code).unwrap_or_default();
    let error = disk_station_error(api, code);
    if error.is_session_error() {
        let _ = var::remove(SID_VAR_KEY);
    }
    Err(error)
}

fn http_status_error(status: u16, body: &str) -> DsError {
    let code = match status {
        300..=399 => PluginErrorCode::InvalidConfig,
        401 | 403 => PluginErrorCode::AuthFailed,
        404 => PluginErrorCode::InvalidConfig,
        429 => PluginErrorCode::RateLimited,
        500..=599 => PluginErrorCode::Temporary,
        _ => PluginErrorCode::Permanent,
    };
    let message = match code {
        PluginErrorCode::AuthFailed => "Download Station rejected the credentials".to_string(),
        PluginErrorCode::InvalidConfig => {
            format!("Download Station is not reachable at this address (HTTP {status})")
        }
        _ => format!("Download Station returned HTTP {status}"),
    };
    DsError::new(code, message).with_debug(format!("HTTP {status}: {}", truncate(body, 300)))
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect::<String>() + "…"
}

fn user_agent() -> String {
    format!(
        "scryer-downloadstation-plugin/{}",
        env!("CARGO_PKG_VERSION")
    )
}

fn add_url(
    config: &DsConfig,
    apis: &ApiSelection,
    url: &str,
    destination: Option<&str>,
) -> Result<(), DsError> {
    let (version, mut params) = if apis.task_v2 {
        // Proxies/DownloadStationTaskProxyV2.cs:43-56.
        (
            2,
            vec![
                ("type".to_string(), "url".to_string()),
                ("url".to_string(), url.to_string()),
                ("create_list".to_string(), "false".to_string()),
            ],
        )
    } else {
        // Proxies/DownloadStationTaskProxyV1.cs:38-48.
        (3, vec![("uri".to_string(), url.to_string())])
    };
    if let Some(destination) = destination.filter(|value| !value.is_empty()) {
        params.push(("destination".to_string(), destination.to_string()));
    }
    let _: serde_json::Value = api_call(
        config,
        apis,
        ApiCall {
            api: apis.task_api(),
            info: &apis.task,
            method: "create",
            version,
            params,
            auth: true,
        },
    )?;
    Ok(())
}

struct FileUpload<'a> {
    file_name: &'a str,
    bytes_base64: &'a str,
    destination: Option<&'a str>,
    content_type: &'a str,
    payload_label: &'a str,
}

fn add_file(config: &DsConfig, apis: &ApiSelection, upload: FileUpload<'_>) -> Result<(), DsError> {
    let bytes = STANDARD.decode(upload.bytes_base64).map_err(|error| {
        DsError::new(
            PluginErrorCode::Permanent,
            format!("invalid {}", upload.payload_label),
        )
        .with_debug(error.to_string())
    })?;
    let destination = upload.destination.filter(|value| !value.is_empty());
    let mut fields = if apis.task_v2 {
        // Proxies/DownloadStationTaskProxyV2.cs:25-41.
        vec![
            (
                "api".to_string(),
                DsApi::DownloadStation2Task.name().to_string(),
            ),
            ("version".to_string(), "2".to_string()),
            ("method".to_string(), "create".to_string()),
            ("type".to_string(), "\"file\"".to_string()),
            ("file".to_string(), "[\"fileData\"]".to_string()),
            ("create_list".to_string(), "false".to_string()),
        ]
    } else {
        // Proxies/DownloadStationTaskProxyV1.cs:24-36.
        vec![
            (
                "api".to_string(),
                DsApi::DownloadStationTask.name().to_string(),
            ),
            ("version".to_string(), "2".to_string()),
            ("method".to_string(), "create".to_string()),
        ]
    };
    if let Some(destination) = destination {
        fields.push((
            "destination".to_string(),
            if apis.task_v2 {
                format!("\"{destination}\"")
            } else {
                destination.to_string()
            },
        ));
    }
    api_multipart(
        config,
        apis,
        MultipartUpload {
            api: apis.task_api(),
            info: &apis.task,
            // v1 carries `_sid` as a form field, v2 in the query string
            // (Proxies/DiskStationProxyBase.cs:173-183).
            sid_in_query: apis.task_v2,
            fields: &fields,
            file_field: if apis.task_v2 { "fileData" } else { "file" },
            file_name: upload.file_name,
            content_type: upload.content_type,
            file_bytes: &bytes,
        },
    )?;
    Ok(())
}

struct MultipartUpload<'a> {
    api: DsApi,
    info: &'a ApiInfo,
    sid_in_query: bool,
    fields: &'a [(String, String)],
    file_field: &'a str,
    file_name: &'a str,
    content_type: &'a str,
    file_bytes: &'a [u8],
}

fn api_multipart(
    config: &DsConfig,
    apis: &ApiSelection,
    upload: MultipartUpload<'_>,
) -> Result<serde_json::Value, DsError> {
    let boundary = "scryer-downloadstation-boundary";
    let mut force_login = false;
    loop {
        let sid = authenticate(config, &apis.auth, force_login)?;
        let url = if upload.sid_in_query {
            format!(
                "{}/webapi/{}?_sid={}",
                config.base_url,
                upload.info.path.trim_start_matches('/'),
                urlencoding::encode(&sid)
            )
        } else {
            format!(
                "{}/webapi/{}",
                config.base_url,
                upload.info.path.trim_start_matches('/')
            )
        };
        let mut body = Vec::new();
        if !upload.sid_in_query {
            write_form_field(&mut body, boundary, "_sid", &sid);
        }
        for (key, value) in upload.fields {
            write_form_field(&mut body, boundary, key, value);
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                upload.file_field,
                upload.file_name.replace('"', "")
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", upload.content_type).as_bytes());
        body.extend_from_slice(upload.file_bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let request = HttpRequest::new(url)
            .with_method("POST")
            .with_header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .with_header("User-Agent", user_agent());
        let response = http::request::<Vec<u8>>(&request, Some(body)).map_err(|error| {
            DsError::new(
                PluginErrorCode::UpstreamUnavailable,
                "Unable to connect to Diskstation, please check your settings",
            )
            .with_debug(error.to_string())
        })?;
        let status = response.status_code();
        let text = String::from_utf8_lossy(&response.body()).to_string();
        if status >= 300 {
            return Err(http_status_error(status, &text));
        }
        match parse_disk_response::<serde_json::Value>(upload.api, &text) {
            Err(error) if error.is_session_error() && !force_login => {
                let _ = var::remove(SID_VAR_KEY);
                force_login = true;
            }
            other => return other,
        }
    }
}

fn write_form_field(body: &mut Vec<u8>, boundary: &str, key: &str, value: &str) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{key}\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

fn list_tasks(config: &DsConfig, apis: &ApiSelection) -> Result<Vec<DsTask>, DsError> {
    if apis.task_v2 {
        // Proxies/DownloadStationTaskProxyV2.cs:59-101: `detail` first, then
        // `transfer`, merged by task id.
        let detail: TaskListV2 = api_call(
            config,
            apis,
            ApiCall {
                api: DsApi::DownloadStation2Task,
                info: &apis.task,
                method: "list",
                version: 1,
                params: vec![("additional".to_string(), "detail".to_string())],
                auth: true,
            },
        )?;
        if detail.total <= 0 {
            return Ok(Vec::new());
        }
        let transfer: TaskListV2 = api_call(
            config,
            apis,
            ApiCall {
                api: DsApi::DownloadStation2Task,
                info: &apis.task,
                method: "list",
                version: 1,
                params: vec![("additional".to_string(), "transfer".to_string())],
                auth: true,
            },
        )?;
        let transfer_by_id = transfer
            .task
            .into_iter()
            .map(|task| (task.id.clone(), task.additional.transfer))
            .collect::<HashMap<_, _>>();
        return Ok(detail
            .task
            .into_iter()
            .map(|task| DsTask {
                id: task.id.clone(),
                title: task.title,
                size: task.size,
                task_type: task.task_type,
                status: status_from_int(task.status),
                // `DownloadStation2Task` carries no `status_extra`, so the v2
                // path has no extraction progress or error detail to report —
                // the same gap Sonarr's v2 proxy has.
                status_extra: HashMap::new(),
                additional: DsAdditional {
                    detail: task.additional.detail,
                    transfer: transfer_by_id.get(&task.id).cloned().unwrap_or_default(),
                },
            })
            .filter(is_supported_task_type)
            .collect());
    }
    let list: TaskListV1 = api_call(
        config,
        apis,
        ApiCall {
            api: DsApi::DownloadStationTask,
            info: &apis.task,
            method: "list",
            version: 1,
            params: vec![("additional".to_string(), "detail,transfer".to_string())],
            auth: true,
        },
    )?;
    Ok(list
        .tasks
        .into_iter()
        .filter(is_supported_task_type)
        .collect())
}

/// Sonarr's `GetDownloadDirectory` (TorrentDownloadStation.cs:449-464).
///
/// `None` is Sonarr's `null`: no configured directory and no Download Station
/// default destination. It is only a validation failure in `TestOutputPath`; an
/// add simply omits the destination and lets Download Station choose.
fn download_directory(config: &DsConfig, apis: &ApiSelection) -> Result<Option<String>, DsError> {
    if !config.directory.trim().is_empty() {
        return Ok(Some(
            config.directory.trim().trim_start_matches('/').to_string(),
        ));
    }
    let Some(info) = apis.info.as_ref() else {
        return Ok(None);
    };
    let data: HashMap<String, serde_json::Value> = api_call(
        config,
        apis,
        ApiCall {
            api: DsApi::DownloadStationInfo,
            info,
            method: "getConfig",
            version: 1,
            params: Vec::new(),
            auth: true,
        },
    )?;
    let default_destination = data
        .get("default_destination")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    if default_destination.is_empty() {
        return Ok(None);
    }
    if config.category.trim().is_empty() {
        return Ok(Some(default_destination));
    }
    Ok(Some(format!(
        "{default_destination}/{}",
        config.category.trim()
    )))
}

// ---------------------------------------------------------------------------
// Serial number / DSM info
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DsmInfo {
    serial_hash: String,
    version: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CachedDsmInfo {
    endpoint: String,
    expires_at: i64,
    info: DsmInfo,
}

/// The hashed serial number that prefixes every `client_item_id`.
///
/// Sonarr throws when it cannot be read (SerialNumberProvider.cs:37-41, pinned
/// by `GetItems_should_throw_if_serial_number_unavailable` and
/// `Download_should_throw_and_not_add_task_if_cannot_get_serial_number`),
/// because the prefix is what keeps items addressable across DSM restarts. The
/// port used to fall back to the literal `"downloadstation"`, so one transient
/// DSM hiccup silently re-keyed every tracked download.
fn dsm_info(config: &DsConfig, apis: &ApiSelection) -> Result<DsmInfo, DsError> {
    if let Some(cached) = var::get::<CachedDsmInfo>(DSM_VAR_KEY).ok().flatten()
        && cached.endpoint == config.base_url
        && cached.expires_at > now_unix_seconds()
    {
        return Ok(cached.info);
    }
    let Some(info) = apis.dsm_info.as_ref() else {
        return Err(DsError::new(
            PluginErrorCode::Unsupported,
            "This DiskStation does not expose SYNO.DSM.Info, so Scryer cannot derive a stable download id",
        ));
    };
    let data: HashMap<String, serde_json::Value> = api_call(
        config,
        apis,
        ApiCall {
            api: DsApi::DsmInfo,
            info,
            method: "getinfo",
            version: info.min_version,
            params: Vec::new(),
            auth: true,
        },
    )?;
    let serial = data
        .get("serial")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    if serial.is_empty() {
        return Err(DsError::new(
            PluginErrorCode::UpstreamUnavailable,
            "Could not get the serial number from Download Station",
        ));
    }
    let info = DsmInfo {
        serial_hash: hashed_serial_number(&serial),
        version: data
            .get("version_string")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .filter(|value| !value.is_empty()),
    };
    let _ = var::set(
        DSM_VAR_KEY,
        CachedDsmInfo {
            endpoint: config.base_url.clone(),
            expires_at: now_unix_seconds() + DSM_TTL_SECONDS,
            info: info.clone(),
        },
    );
    Ok(info)
}

/// `HashConverter.GetHash(serial).ToHexString()` — SHA-1 over the UTF-8 bytes
/// (NzbDrone.Common/Crypto/HashConverter.cs:17-23).
///
/// Sonarr renders the digest in upper case; this stays lower case, because the
/// prefix is only ever consumed by Scryer and re-casing it would re-key every
/// already-tracked Download Station item exactly once for no gain.
fn hashed_serial_number(serial: &str) -> String {
    Sha1::digest(serial.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Shared folder resolution
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
struct SharedFolderEntry {
    shared_folder: String,
    physical_path: String,
    expires_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct SharedFolderCache {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    entries: HashMap<String, SharedFolderEntry>,
    #[serde(default, skip)]
    dirty: bool,
}

impl SharedFolderCache {
    fn load(config: &DsConfig) -> Self {
        let mut cache: Self = var::get(SHARED_FOLDERS_VAR_KEY)
            .ok()
            .flatten()
            .unwrap_or_default();
        if cache.endpoint != config.base_url {
            cache = Self::default();
        }
        let now = now_unix_seconds();
        cache.entries.retain(|_, entry| entry.expires_at > now);
        cache.endpoint = config.base_url.clone();
        cache.dirty = false;
        cache
    }

    fn save(&self, config: &DsConfig) {
        if !self.dirty {
            return;
        }
        let _ = var::set(
            SHARED_FOLDERS_VAR_KEY,
            SharedFolderCache {
                endpoint: config.base_url.clone(),
                entries: self.entries.clone(),
                dirty: false,
            },
        );
    }

    fn get(&self, key: &str) -> Option<&SharedFolderEntry> {
        self.entries
            .get(key)
            .filter(|entry| entry.expires_at > now_unix_seconds())
    }

    fn insert(&mut self, key: String, shared_folder: String, physical_path: String) {
        self.entries.insert(
            key,
            SharedFolderEntry {
                shared_folder,
                physical_path,
                expires_at: now_unix_seconds() + SHARED_FOLDER_TTL_SECONDS,
            },
        );
        self.dirty = true;
    }
}

#[derive(Default, Deserialize)]
struct FileStationFileInfo {
    #[serde(default, rename = "isdir")]
    is_dir: bool,
    #[serde(default)]
    additional: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Default, Deserialize)]
struct FileStationListResponse {
    #[serde(default)]
    files: Vec<FileStationFileInfo>,
}

/// `SYNO.FileStation.List getinfo` with `additional=["real_path"]`
/// (Proxies/FileStationProxy.cs:33-42).
fn file_station_info(
    config: &DsConfig,
    apis: &ApiSelection,
    path: &str,
) -> Result<FileStationFileInfo, DsError> {
    let Some(info) = apis.file_station.as_ref() else {
        return Err(DsError::new(
            PluginErrorCode::Unsupported,
            "This DiskStation does not expose SYNO.FileStation.List, so Scryer cannot resolve Download Station's shared folders to real paths",
        ));
    };
    let response: FileStationListResponse = api_call(
        config,
        apis,
        ApiCall {
            api: DsApi::FileStationList,
            info,
            method: "getinfo",
            version: 2,
            params: vec![
                (
                    "path".to_string(),
                    serde_json::to_string(&[path]).unwrap_or_default(),
                ),
                ("additional".to_string(), "[\"real_path\"]".to_string()),
            ],
            auth: true,
        },
    )?;
    response.files.into_iter().next().ok_or_else(|| {
        DsError::new(
            PluginErrorCode::InvalidConfig,
            format!("Download Station's FileStation returned no information for '{path}'"),
        )
    })
}

/// `SharedFolderResolver.RemapToFullPath` (SharedFolderResolver.cs:44-54).
///
/// Download Station reports a task destination as a shared-folder-relative path
/// (`downloads/tv`), which is not a path that exists on the NAS. Resolving the
/// first segment through FileStation's `real_path` turns it into
/// `/volume1/downloads/tv`, which is what a remote path mapping is written
/// against. The mapping is cached per serial number + shared folder for an hour,
/// as Sonarr does.
fn remap_to_full_path(
    config: &DsConfig,
    apis: &ApiSelection,
    serial: &str,
    cache: &mut SharedFolderCache,
    path: &str,
) -> Result<String, DsError> {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let shared_folder = match path[1..].find('/') {
        Some(index) => path[..index + 1].to_string(),
        None => path.clone(),
    };
    let key = format!("{serial}:{shared_folder}");
    let physical_path = match cache.get(&key) {
        Some(entry) => entry.physical_path.clone(),
        None => {
            let info = file_station_info(config, apis, &shared_folder)?;
            let physical_path = info
                .additional
                .as_ref()
                .and_then(|additional| additional.get("real_path"))
                .and_then(value_to_string)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    DsError::new(
                        PluginErrorCode::InvalidConfig,
                        format!(
                            "The Diskstation does not have a Shared Folder with the name '{}', are you sure you specified it correctly?",
                            shared_folder.trim_start_matches('/')
                        ),
                    )
                })?;
            cache.insert(
                key,
                shared_folder.clone(),
                physical_path.trim_end_matches('/').to_string(),
            );
            physical_path.trim_end_matches('/').to_string()
        }
    };
    let remainder = &path[shared_folder.len()..];
    Ok(format!(
        "{}{}",
        physical_path.trim_end_matches('/'),
        remainder
    ))
}

// ---------------------------------------------------------------------------
// Item mapping
// ---------------------------------------------------------------------------

type Resolver<'a> = dyn FnMut(&str) -> Result<String, DsError> + 'a;

/// `TorrentDownloadStation.GetItems` (64-116) merged with
/// `UsenetDownloadStation.GetItems` (60-127).
///
/// One Scryer client covers both of Sonarr's, so the NZB-specific remaining
/// time is computed here in the same list order Sonarr walks: NZB tasks
/// download sequentially, so a task's remaining time is the cumulative
/// remaining size of every non-paused NZB task up to and including it, divided
/// by the global NZB download speed (UsenetDownloadStation.cs:67-116).
fn build_items(
    config: &DsConfig,
    serial: &str,
    tasks: Vec<DsTask>,
    resolve: &mut Resolver<'_>,
) -> Result<Vec<PluginDownloadItem>, DsError> {
    let global_nzb_speed = tasks
        .iter()
        .filter(|task| task.is_nzb() && task.status == DsStatus::Downloading)
        .map(download_speed)
        .sum::<i64>();
    let mut cumulative_nzb_remaining = 0i64;
    let mut items = Vec::new();
    for task in tasks {
        let remaining = remaining_size(&task);
        if task.is_nzb() && task.status != DsStatus::Paused {
            cumulative_nzb_remaining += remaining;
        }
        if !matches_scope(config, &task) {
            continue;
        }
        let state = map_status(&task);
        let eta_seconds = if task.is_nzb() {
            if state == DownloadItemState::Paused {
                None
            } else {
                eta_seconds_for_speed(cumulative_nzb_remaining, global_nzb_speed)
            }
        } else {
            eta_seconds_for_speed(remaining, download_speed(&task))
        };
        let output_dir = resolve_destination(&task, state, resolve)?;
        items.push(build_item(
            config,
            serial,
            &task,
            ItemContext {
                state,
                remaining,
                eta_seconds,
                output_dir,
            },
        ));
    }
    Ok(items)
}

struct ItemContext {
    state: DownloadItemState,
    remaining: i64,
    eta_seconds: Option<i64>,
    output_dir: Option<String>,
}

/// Sonarr only maps an output path for a completed or failed task
/// (`GetItems_should_not_map_outputpath_for_queued_or_downloading_tasks`), and
/// a resolution failure there fails the whole listing
/// (`GetItems_should_throw_if_shared_folder_resolve_fails`). For a task that
/// does not need the path yet, an unresolvable shared folder yields no path at
/// all rather than the shared-folder-relative fiction the port used to report.
fn resolve_destination(
    task: &DsTask,
    state: DownloadItemState,
    resolve: &mut Resolver<'_>,
) -> Result<Option<String>, DsError> {
    let destination = task.destination();
    let needs_output_path = matches!(
        state,
        DownloadItemState::Completed | DownloadItemState::Seeding | DownloadItemState::Failed
    );
    match resolve(&format!("/{destination}")) {
        Ok(path) => Ok(Some(path)),
        Err(error) if needs_output_path => Err(error),
        Err(_) => Ok(None),
    }
}

fn build_item(
    config: &DsConfig,
    serial: &str,
    task: &DsTask,
    context: ItemContext,
) -> PluginDownloadItem {
    let content_path = context
        .output_dir
        .as_ref()
        .map(|dir| join_path(dir, &task.title));
    let needs_output_path = matches!(
        context.state,
        DownloadItemState::Completed | DownloadItemState::Seeding | DownloadItemState::Failed
    );
    PluginDownloadItem {
        client_item_id: format!("{serial}:{}", task.id),
        download_id: None,
        info_hash: None,
        title: task.title.clone(),
        state: context.state,
        message: message(task),
        category: reported_category(config, task),
        remote_output_path: needs_output_path.then(|| content_path.clone()).flatten(),
        torrent: task.is_bt().then(|| PluginTorrentItem {
            save_path: context.output_dir.clone(),
            content_paths: content_path.clone().into_iter().collect(),
            uploaded_bytes: task.transfer_i64("size_uploaded"),
            downloaded_bytes: task.transfer_i64("size_downloaded"),
            upload_rate_bytes_per_second: task.transfer_i64("speed_upload"),
            download_rate_bytes_per_second: task.transfer_i64("speed_download"),
            seed_ratio: seed_ratio(task),
            seed_time_seconds: seed_time_seconds(task),
            raw_status: Some(status_name(task.status).to_string()),
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(task.size),
        remaining_size_bytes: Some(context.remaining),
        eta_seconds: context.eta_seconds,
        progress_percent: if task.size > 0 {
            Some(
                (((task.size - context.remaining) as f64 / task.size as f64) * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as u8,
            )
        } else {
            None
        },
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files: Some(is_data_complete(task)),
        can_remove: derive_can_remove(task),
        removed: Some(false),
        raw_state: Some(status_name(task.status).to_string()),
        completed_at: unix_to_rfc3339(task.detail_i64("completed_time")),
    }
}

fn build_completed(
    config: &DsConfig,
    serial: &str,
    tasks: Vec<DsTask>,
    resolve: &mut Resolver<'_>,
) -> Result<Vec<PluginCompletedDownload>, DsError> {
    let mut downloads = Vec::new();
    for task in tasks {
        if !matches_scope(config, &task) {
            continue;
        }
        // Download Station keeps a torrent in `seeding` until its global BT
        // seeding goal is met, which can be days. Sonarr treats `seeding` as
        // Completed like `finished` (TorrentDownloadStation.cs:253-256); the
        // port only reported `finished`, so a torrent that kept seeding was
        // never offered for import at all.
        if !matches!(task.status, DsStatus::Finished | DsStatus::Seeding) {
            continue;
        }
        let directory = resolve(&format!("/{}", task.destination()))?;
        let path = join_path(&directory, &task.title);
        downloads.push(PluginCompletedDownload {
            client_item_id: format!("{serial}:{}", task.id),
            download_id: None,
            info_hash: None,
            name: task.title.clone(),
            dest_dir: path.clone(),
            category: reported_category(config, &task),
            output_kind: Some(output_kind(&task.title)),
            content_paths: vec![path],
            size_bytes: Some(task.size),
            completed_at: unix_to_rfc3339(task.detail_i64("completed_time")),
            parameters: Vec::new(),
            release_name: None,
        });
    }
    Ok(downloads)
}

/// Report the client's own casing for the category when the destination proves
/// it, falling back to the configured value.
fn reported_category(config: &DsConfig, task: &DsTask) -> Option<String> {
    if config.category.trim().is_empty() {
        return None;
    }
    category_component(&task.destination(), &config.category)
        .map(str::to_string)
        .or_else(|| Some(config.category.clone()))
}

/// Sonarr's scope filter (TorrentDownloadStation.cs:75-89), with `OsPath`'s
/// fragment comparison rather than a raw string prefix: `/downloads` must not
/// match `/downloads-old`.
fn matches_scope(config: &DsConfig, task: &DsTask) -> bool {
    let destination = task.destination();
    if !config.directory.trim().is_empty() {
        return path_is_within(&config.directory, &destination);
    }
    if !config.category.trim().is_empty() {
        return category_component(&destination, &config.category).is_some();
    }
    true
}

fn path_fragments(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|fragment| !fragment.is_empty())
        .collect()
}

fn path_is_within(scope: &str, candidate: &str) -> bool {
    let scope = path_fragments(scope);
    let candidate = path_fragments(candidate);
    candidate.len() >= scope.len()
        && scope
            .iter()
            .zip(candidate.iter())
            .all(|(left, right)| left == right)
}

/// Category matching is case-insensitive and the client's own casing is what is
/// reported back (`memory: download-category-case-sensitivity`).
fn category_component<'a>(destination: &'a str, category: &str) -> Option<&'a str> {
    path_fragments(destination)
        .into_iter()
        .find(|fragment| fragment.eq_ignore_ascii_case(category.trim()))
}

fn join_path(directory: &str, name: &str) -> String {
    format!("{}/{}", directory.trim_end_matches('/'), name)
}

/// Download Station does not say whether a task produced a single file or a
/// folder without a third `additional=file` listing, so this is a guess — but
/// the previous "the last path segment contains a dot" test called every scene
/// release (`Show.S01E01.1080p.WEB`) a file. Only a recognised media or archive
/// extension counts.
fn output_kind(title: &str) -> PluginDownloadOutputKind {
    let extension = title
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some(
            "mkv" | "mp4" | "avi" | "m4v" | "mov" | "wmv" | "mpg" | "mpeg" | "ts" | "m2ts" | "iso"
            | "flv" | "webm" | "rar" | "zip" | "7z" | "nzb" | "torrent",
        ) => PluginDownloadOutputKind::File,
        _ => PluginDownloadOutputKind::Directory,
    }
}

/// Whether the payload is fully downloaded and therefore movable.
fn is_data_complete(task: &DsTask) -> bool {
    matches!(task.status, DsStatus::Finished | DsStatus::Seeding)
        || (task.size > 0 && remaining_size(task) == 0)
}

/// Honest `can_remove` for Synology Download Station.
///
/// Download Station distinguishes `seeding` (payload complete, still being served toward the
/// global BT seeding ratio/interval) from `finished` (Download Station stopped seeding it).
/// The goal values are not exposed per task, but the transition between those two states is
/// the client's own verdict on whether the obligation is discharged.
///
/// An NZB task has no seeding obligation at all, which is why Sonarr's usenet
/// client reports `CanBeRemoved = true` unconditionally
/// (UsenetDownloadStation.cs:109-110); the honest tri-state version of that is
/// `Some(true)` once the data is complete and `Some(false)` while it is not.
fn derive_can_remove(task: &DsTask) -> Option<bool> {
    if task.is_nzb() {
        return Some(is_data_complete(task));
    }
    match task.status {
        DsStatus::Finished => Some(true),
        DsStatus::Seeding => Some(false),
        _ if is_data_complete(task) => None,
        _ => Some(false),
    }
}

/// Seconds spent seeding, from the task's `seedelapsed` detail field when present.
fn seed_time_seconds(task: &DsTask) -> Option<i64> {
    task.detail_i64("seedelapsed").filter(|value| *value >= 0)
}

/// Sonarr's `GetStatus` (TorrentDownloadStation.cs:243-261), with two places
/// where Scryer's richer state set says more than Sonarr's could.
fn map_status(task: &DsTask) -> DownloadItemState {
    match task.status {
        DsStatus::Unknown | DsStatus::Waiting | DsStatus::FilehostingWaiting => {
            if task.size == 0 || remaining_size(task) > 0 {
                DownloadItemState::Queued
            } else {
                DownloadItemState::Completed
            }
        }
        DsStatus::Paused => DownloadItemState::Paused,
        DsStatus::Finished => DownloadItemState::Completed,
        // Sonarr collapses `seeding` into Completed because it has no seeding
        // state; Scryer's `Seeding` maps to the same queue state and keeps the
        // client's own verdict visible.
        DsStatus::Seeding => DownloadItemState::Seeding,
        // Sonarr reports "Extracting: N%" as a Downloading message; Scryer has
        // the state itself and keeps the message.
        DsStatus::Extracting => DownloadItemState::Extracting,
        DsStatus::HashChecking => DownloadItemState::Verifying,
        DsStatus::Error => DownloadItemState::Failed,
        _ => DownloadItemState::Downloading,
    }
}

/// Sonarr's `GetMessage` (TorrentDownloadStation.cs:223-241).
fn message(task: &DsTask) -> Option<String> {
    if task.status == DsStatus::Extracting {
        return task
            .status_extra_string("unzip_progress")
            .map(|value| format!("Extracting: {value}%"));
    }
    if task.status == DsStatus::Error {
        return task.status_extra_string("error_detail");
    }
    None
}

fn remaining_size(task: &DsTask) -> i64 {
    task.size
        - task
            .transfer_i64("size_downloaded")
            .unwrap_or_default()
            .max(0)
}

fn download_speed(task: &DsTask) -> i64 {
    task.transfer_i64("speed_download")
        .unwrap_or_default()
        .max(0)
}

fn eta_seconds_for_speed(remaining: i64, speed: i64) -> Option<i64> {
    (speed > 0).then(|| remaining / speed)
}

fn seed_ratio(task: &DsTask) -> Option<f64> {
    let downloaded = task.transfer_i64("size_downloaded")?;
    let uploaded = task.transfer_i64("size_uploaded")?;
    Some(if downloaded <= 0 {
        0.0
    } else {
        uploaded as f64 / downloaded as f64
    })
}

fn parse_download_id(id: &str) -> String {
    id.split(':').next_back().unwrap_or(id).to_string()
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

fn source_url(request: &PluginDownloadClientAddRequest) -> Option<String> {
    let source = &request.source;
    let candidate = match source.kind {
        DownloadInputKind::MagnetUri => source
            .magnet_uri
            .clone()
            .or_else(|| source.download_url.clone()),
        DownloadInputKind::TorrentUrl
        | DownloadInputKind::TorrentFile
        | DownloadInputKind::TorrentBytes => source
            .torrent_url
            .clone()
            .or_else(|| source.download_url.clone())
            .or_else(|| source.magnet_uri.clone()),
        // `PluginDownloadSource` has no `nzb_url`: an NZB that is not supplied
        // as bytes arrives as `download_url`.
        DownloadInputKind::Nzb | DownloadInputKind::NzbUrl => source.download_url.clone(),
    };
    candidate.filter(|value| !value.trim().is_empty())
}

/// The name the uploaded file is given, which is also what Download Station
/// stores as the task's `uri` and therefore what identifies the created task.
/// The port's `download.torrent` / `download.nzb` placeholders made every
/// upload indistinguishable.
fn upload_file_name(request: &PluginDownloadClientAddRequest, extension: &str) -> String {
    let supplied = if extension == ".nzb" {
        request.source.nzb_file_name.as_deref()
    } else {
        request.source.torrent_file_name.as_deref()
    };
    let base = supplied
        .map(str::to_string)
        .or_else(|| request.release.release_title.clone())
        .or_else(|| request.source.source_title.clone())
        .map(|value| clean_file_name(&value))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "download".to_string());
    if base.to_ascii_lowercase().ends_with(extension) {
        base
    } else {
        format!("{base}{extension}")
    }
}

fn strip_extension(file_name: &str) -> String {
    match file_name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => file_name.to_string(),
    }
}

fn clean_file_name(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '+',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn normalize_hash(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

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

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn unix_to_rfc3339(value: Option<i64>) -> Option<String> {
    let value = value?;
    if value <= 0 {
        return None;
    }
    let days = value.div_euclid(86_400);
    let seconds_of_day = value.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: DsStatus, downloaded: i64) -> DsTask {
        typed_task("bt", status, downloaded)
    }

    fn typed_task(task_type: &str, status: DsStatus, downloaded: i64) -> DsTask {
        let mut transfer = HashMap::new();
        transfer.insert(
            "size_downloaded".to_string(),
            serde_json::json!(downloaded.to_string()),
        );
        transfer.insert("size_uploaded".to_string(), serde_json::json!("1500"));
        transfer.insert("speed_download".to_string(), serde_json::json!("0"));
        transfer.insert("speed_upload".to_string(), serde_json::json!("0"));
        let mut detail = HashMap::new();
        detail.insert(
            "destination".to_string(),
            serde_json::json!("shared/folder"),
        );
        detail.insert("seedelapsed".to_string(), serde_json::json!(7200));
        DsTask {
            id: "dbid_1".to_string(),
            title: "Movie".to_string(),
            size: 1_000,
            task_type: task_type.to_string(),
            status,
            status_extra: HashMap::new(),
            additional: DsAdditional { detail, transfer },
        }
    }

    fn test_config() -> DsConfig {
        DsConfig {
            base_url: "https://nas.local:5001".to_string(),
            host: "nas.local".to_string(),
            username: "scryer".to_string(),
            password: "secret".to_string(),
            category: String::new(),
            directory: String::new(),
        }
    }

    /// Stands in for the fixtures' `GivenSharedFolder()`, which mocks
    /// `ISharedFolderResolver.RemapToFullPath` to return `_physicalPath` for
    /// **any** input (TorrentDownloadStationFixture.cs:297-302). Keeping the
    /// same simplification is what lets the ported expectations stay literal;
    /// the resolver's own remainder handling is covered by
    /// `a_shared_folder_maps_to_its_physical_path_and_keeps_the_remainder`.
    fn given_shared_folder() -> impl FnMut(&str) -> Result<String, DsError> {
        move |_: &str| Ok("/mnt/sdb1/mydata".to_string())
    }

    /// The real `RemapToFullPath` shape: the first segment is the shared folder
    /// and the remainder is preserved.
    fn resolving_shared_folder() -> impl FnMut(&str) -> Result<String, DsError> {
        move |path: &str| {
            let path = path.trim_start_matches('/');
            Ok(match path.split_once('/') {
                Some((_, rest)) => format!("/mnt/sdb1/mydata/{rest}"),
                None => "/mnt/sdb1/mydata".to_string(),
            })
        }
    }

    fn failing_resolver() -> impl FnMut(&str) -> Result<String, DsError> {
        move |_: &str| {
            Err(DsError::new(
                PluginErrorCode::InvalidConfig,
                "There is no shared folder",
            ))
        }
    }

    fn items(config: &DsConfig, tasks: Vec<DsTask>) -> Vec<PluginDownloadItem> {
        let mut resolve = given_shared_folder();
        build_items(config, "SERIAL", tasks, &mut resolve).expect("items")
    }

    // -----------------------------------------------------------------------
    // Status mapping — ported from
    // `TorrentDownloadStationFixture.GetItems_should_return_item_as_downloadItemStatus`
    // (lines 616-628) and its usenet twin (414-425).
    // -----------------------------------------------------------------------

    #[test]
    fn status_maps_the_way_sonarrs_fixture_table_says() {
        let cases = [
            (DsStatus::Downloading, DownloadItemState::Downloading),
            (DsStatus::Error, DownloadItemState::Failed),
            (DsStatus::Finished, DownloadItemState::Completed),
            (DsStatus::Finishing, DownloadItemState::Downloading),
            (DsStatus::CaptchaNeeded, DownloadItemState::Downloading),
            (DsStatus::Paused, DownloadItemState::Paused),
            (DsStatus::FilehostingWaiting, DownloadItemState::Queued),
            (DsStatus::Waiting, DownloadItemState::Queued),
            (DsStatus::Unknown, DownloadItemState::Queued),
        ];
        for (status, expected) in cases {
            let mut torrent = task(status, 0);
            torrent.size = 1_000;
            assert_eq!(map_status(&torrent), expected, "{status:?}");
        }
    }

    #[test]
    fn richer_states_replace_two_of_sonarrs_downloading_collapses() {
        // Sonarr: Extracting -> Downloading, HashChecking -> Downloading.
        assert_eq!(
            map_status(&task(DsStatus::Extracting, 1_000)),
            DownloadItemState::Extracting
        );
        assert_eq!(
            map_status(&task(DsStatus::HashChecking, 500)),
            DownloadItemState::Verifying
        );
        // Sonarr: Seeding -> Completed. Scryer's Seeding maps to the same
        // queue state core-side.
        assert_eq!(
            map_status(&task(DsStatus::Seeding, 1_000)),
            DownloadItemState::Seeding
        );
    }

    #[test]
    fn waiting_with_no_remaining_size_is_completed() {
        let mut waiting = task(DsStatus::Waiting, 1_000);
        waiting.size = 1_000;
        assert_eq!(map_status(&waiting), DownloadItemState::Completed);
        let mut empty = task(DsStatus::Waiting, 0);
        empty.size = 0;
        assert_eq!(map_status(&empty), DownloadItemState::Queued);
    }

    // -----------------------------------------------------------------------
    // Status parsing — `DownloadStationsTaskStatusJsonConverterFixture`.
    // -----------------------------------------------------------------------

    #[test]
    fn status_strings_parse_the_way_the_converter_fixture_says() {
        for (text, expected) in [
            ("captcha_needed", DsStatus::CaptchaNeeded),
            ("filehosting_waiting", DsStatus::FilehostingWaiting),
            ("hash_checking", DsStatus::HashChecking),
            ("error", DsStatus::Error),
            ("downloading", DsStatus::Downloading),
        ] {
            let task: DsTask =
                serde_json::from_str(&format!("{{\"status\":\"{text}\"}}")).expect("task");
            assert_eq!(task.status, expected, "{text}");
        }
    }

    #[test]
    fn an_unknown_status_string_is_unknown_and_does_not_fail_the_listing() {
        let task: DsTask =
            serde_json::from_str("{\"status\":\"some_unknown_value\"}").expect("task");
        assert_eq!(task.status, DsStatus::Unknown);
        // Contract rule: an unrecognised client state keeps polling.
        assert_eq!(map_status(&task), DownloadItemState::Queued);
    }

    #[test]
    fn additional_values_of_mixed_json_types_still_deserialize() {
        // Download Station is not consistent about these: `size_downloaded` is
        // a string on some DSM builds and a number on others, `completed_time`
        // is always a number. A `HashMap<String, String>` failed the whole
        // task list on any DSM that answered with numbers.
        let raw = r#"{
            "id": "dbid_1",
            "title": "Movie",
            "size": "1000",
            "type": "bt",
            "status": "finished",
            "status_extra": { "unzip_progress": 42 },
            "additional": {
                "detail": { "destination": "shared/folder", "completed_time": 1700000000 },
                "transfer": { "size_downloaded": 1000, "size_uploaded": "100", "speed_download": 0 }
            }
        }"#;
        let task: DsTask = serde_json::from_str(raw).expect("task with mixed value types");
        assert_eq!(task.size, 1_000);
        assert_eq!(task.transfer_i64("size_downloaded"), Some(1_000));
        assert_eq!(task.transfer_i64("size_uploaded"), Some(100));
        assert_eq!(task.detail_i64("completed_time"), Some(1_700_000_000));
        assert_eq!(
            task.status_extra_string("unzip_progress").as_deref(),
            Some("42")
        );
    }

    #[test]
    fn extracting_reports_sonarrs_progress_message() {
        let mut task = task(DsStatus::Extracting, 500);
        task.status_extra
            .insert("unzip_progress".to_string(), serde_json::json!(42));
        assert_eq!(message(&task).as_deref(), Some("Extracting: 42%"));
    }

    #[test]
    fn an_error_task_reports_its_error_detail() {
        let mut task = task(DsStatus::Error, 10);
        task.status_extra.insert(
            "error_detail".to_string(),
            serde_json::json!("destination_denied"),
        );
        assert_eq!(message(&task).as_deref(), Some("destination_denied"));
    }

    // -----------------------------------------------------------------------
    // Output paths — `SharedFolderResolverFixture` and the output-path fixtures.
    // -----------------------------------------------------------------------

    #[test]
    fn output_path_is_the_physical_path_plus_the_title() {
        // `GetItems_should_set_outputPath_to_*` (fixture lines 503-557): all
        // four cases expect `physicalPath + Title`, single file or not.
        let single = {
            let mut task = task(DsStatus::Finished, 1_000);
            task.title = "a.mkv".to_string();
            task
        };
        let multiple = task(DsStatus::Finished, 1_000);
        let mapped = items(&test_config(), vec![single, multiple]);
        assert_eq!(
            mapped[0].remote_output_path.as_deref(),
            Some("/mnt/sdb1/mydata/a.mkv")
        );
        assert_eq!(
            mapped[1].remote_output_path.as_deref(),
            Some("/mnt/sdb1/mydata/Movie")
        );
    }

    #[test]
    fn output_path_is_only_mapped_for_completed_seeding_or_failed_tasks() {
        let mapped = items(
            &test_config(),
            vec![task(DsStatus::Waiting, 0), task(DsStatus::Downloading, 100)],
        );
        assert!(mapped.iter().all(|item| item.remote_output_path.is_none()));

        let mapped = items(
            &test_config(),
            vec![
                task(DsStatus::Finished, 1_000),
                task(DsStatus::Error, 10),
                task(DsStatus::Seeding, 1_000),
            ],
        );
        assert!(mapped.iter().all(|item| item.remote_output_path.is_some()));
    }

    #[test]
    fn a_shared_folder_resolution_failure_fails_a_completed_task_but_not_a_running_one() {
        let mut resolve = failing_resolver();
        // `GetItems_should_throw_if_shared_folder_resolve_fails`.
        assert!(
            build_items(
                &test_config(),
                "SERIAL",
                vec![task(DsStatus::Finished, 1_000)],
                &mut resolve
            )
            .is_err()
        );
        // A task that has no output path to report yet keeps polling instead of
        // failing the whole listing, and reports no path rather than the
        // shared-folder-relative fiction.
        let running = build_items(
            &test_config(),
            "SERIAL",
            vec![task(DsStatus::Downloading, 100)],
            &mut resolve,
        )
        .expect("running task still maps");
        assert_eq!(running[0].remote_output_path, None);
        assert!(
            running[0]
                .torrent
                .as_ref()
                .unwrap()
                .content_paths
                .is_empty()
        );
    }

    #[test]
    fn content_paths_and_save_path_use_the_resolved_physical_path() {
        let mapped = items(&test_config(), vec![task(DsStatus::Finished, 1_000)]);
        let torrent = mapped[0].torrent.as_ref().expect("torrent view");
        assert_eq!(torrent.save_path.as_deref(), Some("/mnt/sdb1/mydata"));
        assert_eq!(torrent.content_paths, vec!["/mnt/sdb1/mydata/Movie"]);
    }

    // -----------------------------------------------------------------------
    // Scope filter — `GetItems_should_ignore_downloads_in_wrong_folder`.
    // -----------------------------------------------------------------------

    #[test]
    fn a_task_outside_the_configured_directory_is_ignored() {
        let config = DsConfig {
            directory: "/shared/folder/sub".to_string(),
            ..test_config()
        };
        assert!(items(&config, vec![task(DsStatus::Finished, 1_000)]).is_empty());
    }

    #[test]
    fn the_directory_filter_compares_path_fragments_not_string_prefixes() {
        assert!(path_is_within("downloads", "downloads/tv"));
        assert!(path_is_within("/downloads", "downloads"));
        assert!(!path_is_within("downloads", "downloads-old"));
        assert!(!path_is_within("downloads/tv", "downloads"));
    }

    #[test]
    fn the_category_filter_is_case_insensitive_and_reports_the_clients_casing() {
        let config = DsConfig {
            category: "sonarr".to_string(),
            ..test_config()
        };
        let mut task = task(DsStatus::Finished, 1_000);
        task.additional.detail.insert(
            "destination".to_string(),
            serde_json::json!("volume1/Sonarr"),
        );
        assert!(matches_scope(&config, &task));
        assert_eq!(reported_category(&config, &task).as_deref(), Some("Sonarr"));
    }

    #[test]
    fn a_task_of_an_unknown_type_is_not_listed() {
        assert!(!is_supported_task_type(&typed_task(
            "ipfs",
            DsStatus::Finished,
            1_000
        )));
        assert!(is_supported_task_type(&typed_task(
            "NZB",
            DsStatus::Finished,
            1_000
        )));
    }

    // -----------------------------------------------------------------------
    // Removal / move semantics.
    // -----------------------------------------------------------------------

    #[test]
    fn can_remove_is_false_while_downloading() {
        let torrent = task(DsStatus::Downloading, 400);
        assert_eq!(derive_can_remove(&torrent), Some(false));
        assert!(!is_data_complete(&torrent));
    }

    #[test]
    fn can_remove_is_false_while_download_station_is_seeding() {
        assert_eq!(
            derive_can_remove(&task(DsStatus::Seeding, 1_000)),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_true_once_download_station_reports_finished() {
        assert_eq!(
            derive_can_remove(&task(DsStatus::Finished, 1_000)),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_unknown_for_a_paused_complete_task() {
        assert_eq!(derive_can_remove(&task(DsStatus::Paused, 1_000)), None);
    }

    #[test]
    fn an_nzb_task_has_no_seeding_obligation_to_discharge() {
        assert_eq!(
            derive_can_remove(&typed_task("nzb", DsStatus::Finished, 1_000)),
            Some(true)
        );
        assert_eq!(
            derive_can_remove(&typed_task("nzb", DsStatus::Seeding, 1_000)),
            Some(true)
        );
        assert_eq!(
            derive_can_remove(&typed_task("nzb", DsStatus::Downloading, 100)),
            Some(false)
        );
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let mapped = items(&test_config(), vec![task(DsStatus::Seeding, 1_000)]);
        assert_eq!(mapped[0].can_move_files, Some(true));
        assert_eq!(mapped[0].can_remove, Some(false));
    }

    #[test]
    fn seed_time_comes_from_the_seedelapsed_detail_field() {
        let with_value = task(DsStatus::Seeding, 1_000);
        assert_eq!(seed_time_seconds(&with_value), Some(7_200));
        let mut without_value = task(DsStatus::Seeding, 1_000);
        without_value.additional.detail.remove("seedelapsed");
        assert_eq!(seed_time_seconds(&without_value), None);
    }

    #[test]
    fn is_private_is_never_claimed_because_download_station_does_not_report_it() {
        let mapped = items(&test_config(), vec![task(DsStatus::Finished, 1_000)]);
        assert_eq!(mapped[0].torrent.as_ref().unwrap().is_private, None);
    }

    #[test]
    fn an_nzb_task_has_no_torrent_view() {
        let mapped = items(
            &test_config(),
            vec![typed_task("nzb", DsStatus::Finished, 1_000)],
        );
        assert!(mapped[0].torrent.is_none());
    }

    // -----------------------------------------------------------------------
    // Remaining time.
    // -----------------------------------------------------------------------

    #[test]
    fn zero_speed_does_not_evaluate_the_division() {
        assert_eq!(eta_seconds_for_speed(10, 0), None);
        assert_eq!(eta_seconds_for_speed(10, 2), Some(5));
    }

    #[test]
    fn item_mapping_does_not_trap_on_a_zero_download_speed() {
        let mapped = items(&test_config(), vec![task(DsStatus::Seeding, 1_000)]);
        assert_eq!(mapped[0].eta_seconds, None);
    }

    #[test]
    fn a_torrents_eta_uses_its_own_speed() {
        let mut torrent = task(DsStatus::Downloading, 100);
        torrent
            .additional
            .transfer
            .insert("speed_download".to_string(), serde_json::json!(50));
        let mapped = items(&test_config(), vec![torrent]);
        assert_eq!(mapped[0].eta_seconds, Some(18));
    }

    #[test]
    fn nzb_tasks_share_the_global_speed_and_accumulate_remaining_size() {
        // UsenetDownloadStation.cs:67-116: NZB tasks download sequentially, so
        // the remaining time of the second task covers the first one too.
        let mut first = typed_task("nzb", DsStatus::Downloading, 0);
        first
            .additional
            .transfer
            .insert("speed_download".to_string(), serde_json::json!(50));
        let mut second = typed_task("nzb", DsStatus::Waiting, 0);
        second.id = "dbid_2".to_string();
        let mapped = items(&test_config(), vec![first, second]);
        assert_eq!(mapped[0].eta_seconds, Some(20));
        assert_eq!(mapped[1].eta_seconds, Some(40));
    }

    #[test]
    fn a_paused_nzb_task_reports_no_remaining_time_and_does_not_add_to_the_total() {
        let mut downloading = typed_task("nzb", DsStatus::Downloading, 0);
        downloading
            .additional
            .transfer
            .insert("speed_download".to_string(), serde_json::json!(50));
        let mut paused = typed_task("nzb", DsStatus::Paused, 0);
        paused.id = "dbid_2".to_string();
        let mut trailing = typed_task("nzb", DsStatus::Waiting, 0);
        trailing.id = "dbid_3".to_string();
        let mapped = items(&test_config(), vec![downloading, paused, trailing]);
        assert_eq!(mapped[1].eta_seconds, None);
        // The paused task's 1000 bytes are not in the running total.
        assert_eq!(mapped[2].eta_seconds, Some(40));
    }

    // -----------------------------------------------------------------------
    // Completed downloads.
    // -----------------------------------------------------------------------

    #[test]
    fn a_seeding_torrent_is_offered_for_import() {
        let mut resolve = given_shared_folder();
        let completed = build_completed(
            &test_config(),
            "SERIAL",
            vec![
                task(DsStatus::Seeding, 1_000),
                task(DsStatus::Finished, 1_000),
                task(DsStatus::Downloading, 100),
            ],
            &mut resolve,
        )
        .expect("completed downloads");
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].dest_dir, "/mnt/sdb1/mydata/Movie");
        assert_eq!(completed[0].content_paths, vec!["/mnt/sdb1/mydata/Movie"]);
    }

    #[test]
    fn a_completed_download_reports_the_clients_completion_time() {
        let mut task = task(DsStatus::Finished, 1_000);
        task.additional.detail.insert(
            "completed_time".to_string(),
            serde_json::json!(1_700_000_000),
        );
        let mut resolve = given_shared_folder();
        let completed =
            build_completed(&test_config(), "SERIAL", vec![task], &mut resolve).expect("completed");
        assert_eq!(
            completed[0].completed_at.as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
    }

    #[test]
    fn unix_timestamps_become_rfc3339_and_zero_stays_absent() {
        assert_eq!(unix_to_rfc3339(None), None);
        assert_eq!(unix_to_rfc3339(Some(0)), None);
        assert_eq!(unix_to_rfc3339(Some(-5)), None);
        assert_eq!(
            unix_to_rfc3339(Some(1_700_000_000)).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
    }

    #[test]
    fn output_kind_no_longer_calls_every_dotted_release_a_file() {
        assert_eq!(
            output_kind("Show.S01E01.1080p.WEB-DL"),
            PluginDownloadOutputKind::Directory
        );
        assert_eq!(output_kind("a.mkv"), PluginDownloadOutputKind::File);
        assert_eq!(output_kind("Movie"), PluginDownloadOutputKind::Directory);
    }

    // -----------------------------------------------------------------------
    // Shared folder resolution shape — `SharedFolderResolverFixture`.
    // -----------------------------------------------------------------------

    #[test]
    fn a_shared_folder_maps_to_its_physical_path_and_keeps_the_remainder() {
        let mut resolve = resolving_shared_folder();
        assert_eq!(resolve("/myFolder").unwrap(), "/mnt/sdb1/mydata");
        assert_eq!(resolve("/myFolder/sub").unwrap(), "/mnt/sdb1/mydata/sub");
    }

    // -----------------------------------------------------------------------
    // Serial number.
    // -----------------------------------------------------------------------

    #[test]
    fn the_serial_number_hash_is_sonarrs_sha1_in_lower_case() {
        // `SerialNumberProviderFixture.should_return_hashedserialnumber` pins
        // "50DE66B735D30738618568294742FCF1DFA52A47" for the serial "serial".
        assert_eq!(
            hashed_serial_number("serial"),
            "50de66b735d30738618568294742fcf1dfa52a47"
        );
    }

    #[test]
    fn a_download_id_is_the_task_id_after_the_serial_prefix() {
        assert_eq!(parse_download_id("SERIAL:dbid_1"), "dbid_1");
        assert_eq!(parse_download_id("dbid_1"), "dbid_1");
    }

    // -----------------------------------------------------------------------
    // Settings validation — `DownloadStationSettingsValidator`.
    // -----------------------------------------------------------------------

    #[test]
    fn a_directory_may_not_start_with_a_slash() {
        let config = DsConfig {
            directory: "/video/Series".to_string(),
            ..test_config()
        };
        let error = validate_settings(&config).expect_err("rejected");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("cannot start with /"));
    }

    #[test]
    fn a_category_allows_only_letters_a_dash_and_a_leading_dot() {
        assert!(category_is_valid(""));
        assert!(category_is_valid("sonarr"));
        assert!(category_is_valid("tv-sonarr"));
        assert!(category_is_valid(".hidden"));
        assert!(category_is_valid("SONARR"));
        assert!(!category_is_valid("sonarr1"));
        assert!(!category_is_valid("tv sonarr"));
    }

    #[test]
    fn a_category_and_a_directory_cannot_be_combined() {
        let config = DsConfig {
            category: "sonarr".to_string(),
            directory: "video/Series".to_string(),
            ..test_config()
        };
        assert_eq!(
            validate_scope_exclusivity(&config)
                .expect_err("rejected")
                .code,
            PluginErrorCode::InvalidConfig
        );
    }

    #[test]
    fn a_routed_directory_outside_the_clients_scope_is_refused() {
        let config = DsConfig {
            directory: "video/Series".to_string(),
            ..test_config()
        };
        assert!(ensure_routed_directory_is_in_scope(&config, "video/Series/Show").is_ok());
        let error =
            ensure_routed_directory_is_in_scope(&config, "video/Movies").expect_err("out of scope");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);

        let config = DsConfig {
            category: "sonarr".to_string(),
            ..test_config()
        };
        assert!(ensure_routed_directory_is_in_scope(&config, "volume1/Sonarr/Show").is_ok());
        assert!(ensure_routed_directory_is_in_scope(&config, "volume1/other").is_err());
    }

    // -----------------------------------------------------------------------
    // Post-add identification.
    // -----------------------------------------------------------------------

    fn task_with_uri(id: &str, uri: &str, task_type: &str) -> DsTask {
        let mut task = typed_task(task_type, DsStatus::Waiting, 0);
        task.id = id.to_string();
        task.additional
            .detail
            .insert("uri".to_string(), serde_json::json!(uri));
        task
    }

    #[test]
    fn a_new_task_wins_over_a_pre_existing_one_with_the_same_uri() {
        let magnet = "magnet:?xt=urn:btih:abc";
        let tasks = vec![
            task_with_uri("old", magnet, "bt"),
            task_with_uri("new", magnet, "bt"),
        ];
        let before = HashSet::from(["old".to_string()]);
        let expected = ExpectedTask {
            uris: vec![magnet.to_string()],
            is_nzb: false,
        };
        assert_eq!(
            identify_added_task(&tasks, &expected, &before).unwrap(),
            "new"
        );
    }

    #[test]
    fn a_deduplicated_add_still_resolves_to_the_existing_task() {
        let magnet = "magnet:?xt=urn:btih:abc";
        let tasks = vec![task_with_uri("old", magnet, "bt")];
        let before = HashSet::from(["old".to_string()]);
        let expected = ExpectedTask {
            uris: vec![magnet.to_string()],
            is_nzb: false,
        };
        assert_eq!(
            identify_added_task(&tasks, &expected, &before).unwrap(),
            "old"
        );
    }

    #[test]
    fn a_renamed_uri_still_resolves_to_the_single_new_task_of_the_right_type() {
        let tasks = vec![
            task_with_uri("old", "other", "bt"),
            task_with_uri("new", "normalised-magnet", "bt"),
            task_with_uri("unrelated", "an-nzb.nzb", "nzb"),
        ];
        let before = HashSet::from(["old".to_string()]);
        let expected = ExpectedTask {
            uris: vec!["magnet:?xt=urn:btih:abc".to_string()],
            is_nzb: false,
        };
        assert_eq!(
            identify_added_task(&tasks, &expected, &before).unwrap(),
            "new"
        );
    }

    #[test]
    fn an_ambiguous_result_is_a_permanent_failure_like_sonarrs_singleordefault() {
        let magnet = "magnet:?xt=urn:btih:abc";
        let tasks = vec![
            task_with_uri("one", magnet, "bt"),
            task_with_uri("two", magnet, "bt"),
        ];
        let expected = ExpectedTask {
            uris: vec![magnet.to_string()],
            is_nzb: false,
        };
        let error = identify_added_task(&tasks, &expected, &HashSet::new()).expect_err("ambiguous");
        assert_eq!(error.code, PluginErrorCode::Permanent);
    }

    #[test]
    fn a_missing_task_is_retryable() {
        let expected = ExpectedTask {
            uris: vec!["magnet:?xt=urn:btih:abc".to_string()],
            is_nzb: false,
        };
        let error = identify_added_task(&[], &expected, &HashSet::new()).expect_err("not found");
        assert_eq!(error.code, PluginErrorCode::Temporary);
    }

    // -----------------------------------------------------------------------
    // Upload names and sources.
    // -----------------------------------------------------------------------

    fn add_request(kind: DownloadInputKind) -> PluginDownloadClientAddRequest {
        serde_json::from_value(serde_json::json!({
            "source": { "kind": kind },
            "release": {},
            "title": { "title_name": "Show", "media_facet": "series" },
            "routing": {}
        }))
        .expect("add request")
    }

    #[test]
    fn an_upload_is_named_after_the_release_instead_of_a_placeholder() {
        let mut request = add_request(DownloadInputKind::Nzb);
        request.release.release_title = Some("Show.S01E01.1080p.WEB-DL".to_string());
        assert_eq!(
            upload_file_name(&request, ".nzb"),
            "Show.S01E01.1080p.WEB-DL.nzb"
        );
        assert_eq!(
            upload_file_name(&request, ".torrent"),
            "Show.S01E01.1080p.WEB-DL.torrent"
        );
    }

    #[test]
    fn an_upload_name_prefers_the_supplied_file_name_and_is_path_safe() {
        let mut request = add_request(DownloadInputKind::Nzb);
        request.source.nzb_file_name = Some("Show S01E01.nzb".to_string());
        assert_eq!(upload_file_name(&request, ".nzb"), "Show S01E01.nzb");

        let mut request = add_request(DownloadInputKind::Nzb);
        request.release.release_title = Some("Show / S01E01: pilot".to_string());
        assert_eq!(
            upload_file_name(&request, ".nzb"),
            "Show + S01E01+ pilot.nzb"
        );
    }

    #[test]
    fn a_torrent_upload_is_matched_without_its_extension_and_an_nzb_with_it() {
        assert_eq!(strip_extension("Show.S01E01.torrent"), "Show.S01E01");
        assert_eq!(strip_extension("Show"), "Show");
    }

    #[test]
    fn an_nzb_url_source_reads_the_sdks_download_url() {
        let mut request = add_request(DownloadInputKind::NzbUrl);
        request.source.download_url = Some("https://indexer.example/nzb/1".to_string());
        assert_eq!(
            source_url(&request).as_deref(),
            Some("https://indexer.example/nzb/1")
        );
    }

    // -----------------------------------------------------------------------
    // DiskStation error tables — `Responses/DiskStationError.cs`.
    // -----------------------------------------------------------------------

    #[test]
    fn diskstation_error_codes_carry_sonarrs_messages() {
        assert_eq!(
            api_error_message(DsApi::Auth, 400),
            "No such account or incorrect password"
        );
        assert_eq!(
            api_error_message(DsApi::DownloadStation2Task, 406),
            "No default destination"
        );
        assert_eq!(
            api_error_message(DsApi::FileStationList, 160),
            "Permission denied. Give your user access to FileStation."
        );
        assert_eq!(
            api_error_message(DsApi::DsmInfo, 105),
            "The logged in session does not have permission"
        );
        assert_eq!(api_error_message(DsApi::Info, 987), "987 - Unknown error");
    }

    #[test]
    fn diskstation_error_codes_are_classified_rather_than_all_temporary() {
        assert_eq!(
            api_error_code(DsApi::Auth, 400),
            PluginErrorCode::AuthFailed
        );
        assert_eq!(
            api_error_code(DsApi::Auth, 403),
            PluginErrorCode::AuthFailed
        );
        assert_eq!(
            api_error_code(DsApi::DsmInfo, 105),
            PluginErrorCode::AuthFailed
        );
        assert_eq!(
            api_error_code(DsApi::DownloadStationTask, 402),
            PluginErrorCode::InvalidConfig
        );
        assert_eq!(
            api_error_code(DsApi::FileStationList, 160),
            PluginErrorCode::InvalidConfig
        );
        assert_eq!(
            api_error_code(DsApi::DownloadStationTask, 104),
            PluginErrorCode::Unsupported
        );
        assert_eq!(
            api_error_code(DsApi::DownloadStationTask, 401),
            PluginErrorCode::Temporary
        );
    }

    #[test]
    fn the_session_error_codes_are_the_ones_that_evict_the_sid() {
        for code in [105, 106, 107, 119] {
            assert!(disk_station_error(DsApi::DownloadStationTask, code).is_session_error());
        }
        assert!(!disk_station_error(DsApi::DownloadStationTask, 402).is_session_error());
    }

    #[test]
    fn an_html_answer_is_a_configuration_problem_not_a_transient_one() {
        let error = parse_disk_response::<serde_json::Value>(
            DsApi::Info,
            "<html><body>Sign in</body></html>",
        )
        .expect_err("not json");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn http_statuses_map_to_distinct_codes() {
        assert_eq!(http_status_error(401, "").code, PluginErrorCode::AuthFailed);
        assert_eq!(
            http_status_error(302, "").code,
            PluginErrorCode::InvalidConfig
        );
        assert_eq!(http_status_error(503, "").code, PluginErrorCode::Temporary);
        assert_eq!(
            http_status_error(429, "").code,
            PluginErrorCode::RateLimited
        );
    }

    #[test]
    fn the_task_api_version_gate_reports_the_range_like_sonarr() {
        let apis = ApiSelection {
            auth: ApiInfo::default(),
            task: ApiInfo {
                min_version: 3,
                max_version: 4,
                path: "entry.cgi".to_string(),
            },
            task_v2: true,
            info: None,
            dsm_info: None,
            file_station: None,
        };
        let error = validate_task_api_version(&apis).expect_err("unsupported");
        assert_eq!(error.code, PluginErrorCode::Unsupported);
        assert!(error.public_message.contains("from 3 to 4"));

        let apis = ApiSelection {
            task: ApiInfo {
                min_version: 1,
                max_version: 2,
                path: "entry.cgi".to_string(),
            },
            ..apis
        };
        assert!(validate_task_api_version(&apis).is_ok());
    }

    // -----------------------------------------------------------------------
    // Descriptor.
    // -----------------------------------------------------------------------

    #[test]
    fn the_descriptor_advertises_only_isolation_download_station_actually_has() {
        let raw = scryer_describe(String::new()).expect("descriptor");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(
            value["provider"]["isolation_modes"],
            serde_json::json!(["category", "directory"])
        );
        assert_eq!(
            value["provider"]["capabilities"]["per_download_directory"],
            serde_json::json!(true)
        );
        assert_eq!(
            value["provider"]["capabilities"]["remove_with_data"],
            serde_json::json!(false)
        );
        assert_eq!(
            value["provider"]["capabilities"]["mark_imported_non_destructive"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn the_config_field_keys_are_unchanged() {
        let keys = config_fields()
            .into_iter()
            .map(|field| field.key)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "host",
                "port",
                "use_ssl",
                "username",
                "password",
                "category",
                "directory"
            ]
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
