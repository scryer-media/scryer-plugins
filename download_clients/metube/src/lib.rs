//! MeTube download client: a yt-dlp web front end driven over its HTTP API.
//!
//! This is an `http`-family client. Scryer hands it a `PluginHttpDownloadSource`
//! whose `release.page_url` is the page yt-dlp resolves itself, and the plugin
//! maps the host's `selected` quality, codec and container preferences onto
//! MeTube's own catalogue (`quality`, `codec`, `format`). `release.resources`
//! are ignored on purpose: yt-dlp resolves its own streams.
//!
//! Observed MeTube contract (`app/main.py`, `app/ytdl.py`,
//! `app/format_catalog.py` at research time):
//!
//! | Operation      | MeTube                                                   |
//! |----------------|----------------------------------------------------------|
//! | Add            | `POST add` with `url`, `download_type`, `quality`, …     |
//! | Queue, history | `GET history` → `{ done: [], queue: [], pending: [] }`   |
//! | Cancel         | `POST delete` with `ids`, `where: "queue"`               |
//! | Clear          | `POST delete` with `ids`, `where: "done"`, `delete_files`|
//! | Version        | `GET version` → `{ "yt-dlp": …, "version": … }`          |
//!
//! MeTube keys every item by its URL (`webpage_url`, falling back to the
//! submitted URL); there is no server-generated job id. The plugin therefore
//! submits exactly `release.page_url`, reads back the key MeTube stored it
//! under (see `find_item_key`), reports that as the `client_item_id`, and uses
//! it for every later lookup and every `delete`.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, DownloadClientCapabilities,
    DownloadClientDescriptor, DownloadControlAction, DownloadHttpCapabilities, DownloadInputKind,
    DownloadItemState, HttpDownloadPhase, HttpSubtitleMode, HttpTransport, PluginCompletedDownload,
    PluginDescriptor, PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginHttpDownloadSource, PluginHttpItem, PluginHttpSelection, PluginResult,
    ProviderDescriptor, SDK_VERSION,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// MeTube's container image listens on 8081.
const DEFAULT_BASE_URL: &str = "http://localhost:8081";
/// MeTube's container image downloads into `/downloads` (`DOWNLOAD_DIR`).
const DEFAULT_DOWNLOAD_DIR: &str = "/downloads";

/// MeTube's video quality ladder (`format_catalog.py`), highest first. The
/// host's `max_height` is mapped onto the tallest rung that does not exceed
/// it.
const QUALITY_LADDER: &[u32] = &[2160, 1440, 1080, 720, 480, 360, 240];
/// Video codecs MeTube offers besides `auto` (`format_catalog.py`).
const CODEC_CHOICES: &[&str] = &["h264", "h265", "av1", "vp9"];
/// Subtitle modes MeTube offers (`format_catalog.py`); the plugin only ever
/// sends the catalogue default, since the host's sidecar modes do not map
/// onto a manual-versus-auto caption choice.
const SUBTITLE_MODE_DEFAULT: &str = "prefer_manual";
/// Video formats the plugin sends (`format_for`).
const FORMAT_CHOICES: &[&str] = &["any", "mp4"];

#[derive(Debug, Clone)]
struct MetubeConfig {
    /// Normalised to end with exactly one `/`, so routes append directly.
    base_url: String,
    username: String,
    password: String,
    /// MeTube's `DOWNLOAD_DIR` as the remote side sees it. A history row's
    /// `folder` is relative to it, and its `filename` relative to that.
    download_dir: String,
    /// Put every download in its own sub-folder named after the host's
    /// `output_basename`. Requires MeTube's `CUSTOM_DIRS` (its default).
    subfolder_per_download: bool,
}

// ---------------------------------------------------------------------------
// Typed errors
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
            "Scryer sent a request this MeTube plugin could not read.",
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

/// The text a failed host HTTP call carries. The command host reports a
/// transport failure as a service error whose public message is generic
/// ("command plugin host service failed") and whose cause, `timeout` among
/// them, is only in `debug_message`; the error's `Display` keeps only the
/// public part. Both are read, so the cause survives into classification.
fn transport_error_detail(error: &scryer_plugin_pdk::Error) -> String {
    match error.downcast_ref::<scryer_plugin_pdk::host::HostCallError>() {
        Some(scryer_plugin_pdk::host::HostCallError::Service(service)) => {
            match service.debug_message.as_deref().map(str::trim) {
                Some(debug) if !debug.is_empty() => {
                    format!("{}: {debug}", service.public_message)
                }
                _ => service.public_message.clone(),
            }
        }
        _ => error.to_string(),
    }
}

/// Classification is by substring of [`transport_error_detail`]. A timeout
/// matters most here: MeTube runs yt-dlp's extraction inside `add` before it
/// answers, so a slow provider can time the add out after MeTube has already
/// queued the item.
fn classify_transport_error(detail: &str) -> PluginError {
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("timeout") || lowered.contains("timed out") {
        detailed_error(
            PluginErrorCode::Temporary,
            "MeTube did not answer in time.",
            detail,
        )
    } else if lowered.contains("certificate")
        || lowered.contains("tls")
        || lowered.contains("ssl")
        || lowered.contains("trust")
    {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "The TLS connection to MeTube could not be established.",
            detail,
        )
    } else {
        detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            "MeTube could not be reached.",
            detail,
        )
    }
}

fn is_timeout(error: &PluginError) -> bool {
    error.code == PluginErrorCode::Temporary
        && error
            .debug_message
            .as_deref()
            .map(|detail| {
                let lowered = detail.to_ascii_lowercase();
                lowered.contains("timeout") || lowered.contains("timed out")
            })
            .unwrap_or(false)
}

fn http_status_error(status: u16, body: &str) -> PluginError {
    let detail = truncate(body);
    match status {
        401 | 407 => detailed_error(
            PluginErrorCode::AuthFailed,
            "MeTube's reverse proxy rejected the credentials.",
            detail,
        ),
        403 => detailed_error(
            PluginErrorCode::Permanent,
            "MeTube refused the request. If MeTube sits behind a proxy that adds an Origin header, add that origin to MeTube's CORS_ALLOWED_ORIGINS.",
            detail,
        ),
        404 => detailed_error(
            PluginErrorCode::InvalidConfig,
            "MeTube was not found at the configured base URL. Check the URL and MeTube's URL_PREFIX.",
            detail,
        ),
        // A proxy in front of MeTube timing out or throttling: worth a retry.
        408 | 429 => detailed_error(
            PluginErrorCode::Temporary,
            format!("MeTube returned HTTP {status}."),
            detail,
        ),
        500..=599 => detailed_error(
            PluginErrorCode::UpstreamUnavailable,
            format!("MeTube returned HTTP {status}."),
            detail,
        ),
        _ => detailed_error(
            PluginErrorCode::Permanent,
            format!("MeTube returned HTTP {status}."),
            detail,
        ),
    }
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "metube".to_string(),
        name: "MeTube".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        settings: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "metube".to_string(),
            provider_aliases: vec![],
            config_fields: config_fields(),
            default_base_url: Some(DEFAULT_BASE_URL.to_string()),
            allowed_hosts: vec![],
            accepted_inputs: vec![DownloadInputKind::HttpResource],
            isolation_modes: vec![],
            capabilities: DownloadClientCapabilities {
                category_scoped_feedback: false,
                // MeTube has no pause; `cancel` discards the item.
                pause: false,
                resume: false,
                remove: true,
                // `delete` with `where: "done"` takes `delete_files`.
                remove_with_data: true,
                // Nothing to label, tag or move on the MeTube side.
                mark_imported: false,
                mark_imported_non_destructive: false,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                seed_limits: false,
                start_paused: false,
                force_start: false,
                // `folder` is a sub-folder under MeTube's own DOWNLOAD_DIR,
                // not an arbitrary directory Scryer can route to.
                per_download_directory: false,
                host_fs_required: false,
                test_connection: true,
                torrent: None,
                http: Some(DownloadHttpCapabilities {
                    // yt-dlp resolves provider pages and fetches direct files,
                    // HLS and DASH alike.
                    transports: vec![
                        HttpTransport::ProviderPage,
                        HttpTransport::DirectFile,
                        HttpTransport::Hls,
                        HttpTransport::Dash,
                    ],
                    providers: vec![],
                    muxes_separate_tracks: true,
                    // Captions are a separate MeTube download type. A video
                    // add fetches no subtitles at all (MeTube validates the
                    // subtitle fields on every add but applies them only to
                    // `captions`), so nothing is written beside the media.
                    writes_subtitle_sidecars: false,
                    reports_final_paths: true,
                }),
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "base_url",
            "Base URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("MeTube's address, including its URL_PREFIX when one is set."),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Only when a reverse proxy in front of MeTube asks for HTTP basic authentication. MeTube itself has no login.",
            ),
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
            "download_dir",
            "Download Directory",
            ConfigFieldType::String,
            true,
            Some(DEFAULT_DOWNLOAD_DIR),
            Some(
                "MeTube's DOWNLOAD_DIR as MeTube sees it. Completed paths are reported under it; use a remote path mapping when Scryer mounts it elsewhere.",
            ),
        ),
        field(
            "subfolder_per_download",
            "Sub-folder per download",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            Some(
                "Place each download in its own sub-folder of the download directory. Requires MeTube's CUSTOM_DIRS (its default).",
            ),
        ),
    ]
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
        ..Default::default()
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

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

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

impl MetubeConfig {
    fn from_host() -> Self {
        Self {
            base_url: normalize_base_url(
                &config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            ),
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            download_dir: config_value("download_dir")
                .unwrap_or_else(|| DEFAULT_DOWNLOAD_DIR.to_string()),
            subfolder_per_download: config_bool("subfolder_per_download", true),
        }
    }

    fn route(&self, name: &str) -> String {
        format!("{}{name}", self.base_url)
    }
}

/// `URL_PREFIX` ends with a slash and the routes are registered as
/// `URL_PREFIX + 'add'`, so the base always ends with exactly one `/`.
fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return format!("{DEFAULT_BASE_URL}/");
    }
    if trimmed.contains("://") {
        format!("{trimmed}/")
    } else {
        format!("http://{trimmed}/")
    }
}

fn is_localhost_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.contains("://localhost") || lower.contains("://127.0.0.1") || lower.contains("://[::1]")
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn send(
    config: &MetubeConfig,
    method: &str,
    route: &str,
    body: Option<&Value>,
) -> Result<HttpResponse, PluginError> {
    let mut request = HttpRequest::new(config.route(route))
        .with_method(method)
        .with_header("Accept", "application/json")
        .with_header(
            "User-Agent",
            concat!("scryer-metube-plugin/", env!("CARGO_PKG_VERSION")),
        );
    if body.is_some() {
        request = request.with_header("Content-Type", "application/json");
    }
    if !config.username.is_empty() || !config.password.is_empty() {
        request = request.with_header(
            "Authorization",
            format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{}", config.username, config.password))
            ),
        );
    }
    let payload = body.map(|value| value.to_string().into_bytes());
    let response = http::request::<Vec<u8>>(&request, payload)
        .map_err(|error| classify_transport_error(&transport_error_detail(&error)))?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(&response.body()).to_string();
        return Err(http_status_error(status, &text));
    }
    Ok(response)
}

fn send_json<T: serde::de::DeserializeOwned>(
    config: &MetubeConfig,
    method: &str,
    route: &str,
    body: Option<&Value>,
) -> Result<T, PluginError> {
    let response = send(config, method, route, body)?;
    let bytes = response.body();
    serde_json::from_slice(&bytes).map_err(|error| {
        detailed_error(
            PluginErrorCode::Permanent,
            format!("MeTube answered `{route}` with something that is not JSON."),
            format!("{error}: {}", truncate(&String::from_utf8_lossy(&bytes))),
        )
    })
}

/// `{ "status": "ok" | "error", "msg": … }`, the shape every mutating MeTube
/// route answers with.
#[derive(Debug, Clone, Deserialize)]
struct MetubeStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    msg: Option<String>,
}

impl MetubeStatus {
    fn is_ok(&self) -> bool {
        self.status.eq_ignore_ascii_case("ok")
    }
}

/// One row of `history`, as `DownloadInfo.to_public_dict()` renders it.
/// Everything is optional and numbers are read as floats, because yt-dlp's
/// progress hooks hand MeTube floats for `percent`, `speed` and `eta`. MeTube
/// sets `size` only once a download finishes (`os.path.getsize`).
///
/// `timestamp` is deliberately not read: MeTube stamps it with
/// `time.time_ns()` when the item is created, so it is neither in seconds nor
/// a completion time.
#[derive(Debug, Clone, Default, Deserialize)]
struct MetubeDownload {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    eta: Option<f64>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default)]
    filename: Option<String>,
    /// The sub-folder of `DOWNLOAD_DIR` the add asked for; `filename` is
    /// relative to it.
    #[serde(default)]
    folder: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MetubeHistory {
    #[serde(default)]
    done: Vec<MetubeDownload>,
    #[serde(default)]
    queue: Vec<MetubeDownload>,
    #[serde(default)]
    pending: Vec<MetubeDownload>,
}

fn fetch_history(config: &MetubeConfig) -> Result<MetubeHistory, PluginError> {
    send_json(config, "GET", "history", None)
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MetubeVersion {
    #[serde(default)]
    version: Option<String>,
    #[serde(default, rename = "yt-dlp")]
    yt_dlp: Option<String>,
}

fn fetch_version(config: &MetubeConfig) -> Result<String, PluginError> {
    let version: MetubeVersion = send_json(config, "GET", "version", None)?;
    let metube = version
        .version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let yt_dlp = version
        .yt_dlp
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (metube, yt_dlp) {
        (Some(metube), Some(yt_dlp)) => Ok(format!("{metube} (yt-dlp {yt_dlp})")),
        (Some(metube), None) => Ok(metube.to_string()),
        (None, Some(yt_dlp)) => Ok(format!("yt-dlp {yt_dlp}")),
        (None, None) => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "The configured base URL answered, but not as MeTube: its `version` route reported no version.",
        )),
    }
}

// ---------------------------------------------------------------------------
// Add
// ---------------------------------------------------------------------------

pub fn scryer_download_add(input: String) -> FnResult<String> {
    respond(add(&input))
}

fn add(input: &str) -> Result<PluginDownloadClientAddResponse, PluginError> {
    let request: PluginDownloadClientAddRequest = parse_request(input)?;
    let config = MetubeConfig::from_host();
    let http = http_source(&request)?;
    let page_url = page_url(http)?;
    let body = build_add_body(http, &config);

    let item_id = http.release.item_id.trim();

    let client_item_id = match send_json::<MetubeStatus>(&config, "POST", "add", Some(&body)) {
        // MeTube keys the item by yt-dlp's `webpage_url`, which is not always
        // the URL that was submitted. Read the key back so every later lookup
        // and `delete` names the item MeTube actually holds. The add has
        // already succeeded, so a failed read falls back to the page URL
        // instead of failing it.
        Ok(status) if status.is_ok() => fetch_history(&config)
            .ok()
            .and_then(|history| find_item_key(&history, page_url, item_id))
            .unwrap_or_else(|| page_url.to_string()),
        Ok(status) => return Err(add_refused(status.msg)),
        Err(error) if is_timeout(&error) => {
            // MeTube runs yt-dlp's extraction inside `add`, so a timeout does
            // not mean the item was not queued. Reconcile against `history`
            // rather than resubmitting blindly.
            let history = fetch_history(&config).map_err(|_| error.clone())?;
            match find_item_key(&history, page_url, item_id) {
                Some(key) => key,
                None => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };

    Ok(PluginDownloadClientAddResponse {
        client_item_id,
        info_hash: None,
    })
}

/// A `{"status": "error"}` reply to `add`. Most are yt-dlp extraction
/// failures for this one item (removed, private, unsupported URL), which no
/// retry fixes. A throttled or unreachable provider is worth another try, and
/// a `folder` MeTube will not create is the plugin's configuration, not the
/// release.
fn add_refused(message: Option<String>) -> PluginError {
    let message = message
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| "MeTube reported an error without a message.".to_string());
    let lowered = message.to_ascii_lowercase();
    let code = if lowered.contains("custom_dirs") || lowered.contains("base download directory") {
        PluginErrorCode::InvalidConfig
    } else if lowered.contains("429")
        || lowered.contains("too many requests")
        || lowered.contains("timed out")
        || lowered.contains("temporarily")
        || lowered.contains("connection reset")
    {
        PluginErrorCode::Temporary
    } else {
        PluginErrorCode::Permanent
    };
    let public = if code == PluginErrorCode::InvalidConfig {
        format!(
            "MeTube refused the download folder: {}. Turn off \"Sub-folder per download\" or enable MeTube's CUSTOM_DIRS and CREATE_CUSTOM_DIRS.",
            truncate(&message)
        )
    } else {
        format!("MeTube refused the download: {}", truncate(&message))
    };
    detailed_error(code, public, message)
}

fn http_source(
    request: &PluginDownloadClientAddRequest,
) -> Result<&PluginHttpDownloadSource, PluginError> {
    if request.source.kind != DownloadInputKind::HttpResource {
        return Err(plugin_error(
            PluginErrorCode::Unsupported,
            "MeTube only takes HTTP releases; this download is not one.",
        ));
    }
    request.source.http.as_ref().ok_or_else(|| {
        plugin_error(
            PluginErrorCode::Permanent,
            "Scryer sent an HTTP download without its release details.",
        )
    })
}

fn page_url(http: &PluginHttpDownloadSource) -> Result<&str, PluginError> {
    let url = http.release.page_url.trim();
    if url.is_empty() {
        return Err(plugin_error(
            PluginErrorCode::Permanent,
            "The release has no page URL for yt-dlp to resolve.",
        ));
    }
    Ok(url)
}

/// The `POST add` body. Only fields MeTube validates leniently are sent, and
/// `ytdl_options_overrides` never is: MeTube rejects it unless
/// `ALLOW_YTDL_OPTIONS_OVERRIDES` is on, and its README warns it allows
/// command execution in the container.
fn build_add_body(http: &PluginHttpDownloadSource, config: &MetubeConfig) -> Value {
    let selected = &http.selected;
    let mut body = json!({
        "url": http.release.page_url.trim(),
        "download_type": "video",
        "quality": quality_for(selected.max_height),
        "codec": codec_for(&selected.video_codec_preference),
        "format": format_for(&selected.container_preference),
        // Single-item policy: only watch URLs are submitted, and the limit
        // keeps a page that turns out to be a list from expanding.
        "playlist_item_limit": 1,
        "auto_start": true,
        "custom_name_prefix": "",
    });
    if config.subfolder_per_download {
        body["folder"] = Value::String(folder_for(&http.policy.output_basename));
    }
    // MeTube validates these on every add but, for a `video` download,
    // currently fetches no subtitles with them (`dl_formats.get_opts` reads
    // them only for `captions`). They are sent so the request says what the
    // host asked for, and sanitised so they can never fail validation.
    if let Some(language) = subtitle_language_for(selected) {
        body["subtitle_language"] = Value::String(language);
        body["subtitle_mode"] = Value::String(SUBTITLE_MODE_DEFAULT.to_string());
    }
    body
}

/// MeTube's `quality`: the tallest rung of its ladder that does not exceed the
/// host's ceiling, `best` without a ceiling, `worst` below the ladder.
fn quality_for(max_height: Option<u32>) -> String {
    let Some(max_height) = max_height else {
        return "best".to_string();
    };
    QUALITY_LADDER
        .iter()
        .find(|rung| **rung <= max_height)
        .map(|rung| rung.to_string())
        .unwrap_or_else(|| "worst".to_string())
}

/// The first host codec preference MeTube offers, else `auto`.
fn codec_for(preference: &[String]) -> String {
    preference
        .iter()
        .map(|codec| codec.trim().to_ascii_lowercase())
        .find(|codec| CODEC_CHOICES.contains(&codec.as_str()))
        .unwrap_or_else(|| "auto".to_string())
}

/// MeTube offers no mkv or webm choice, only `mp4`, `ios` and `any`: `mp4`
/// when it is the host's first container preference, otherwise yt-dlp picks.
fn format_for(preference: &[String]) -> String {
    match preference
        .first()
        .map(|container| container.trim().to_ascii_lowercase())
    {
        Some(container) if container == "mp4" => "mp4".to_string(),
        _ => "any".to_string(),
    }
}

fn subtitle_language_for(selected: &PluginHttpSelection) -> Option<String> {
    if matches!(selected.subtitle_mode, HttpSubtitleMode::None) {
        return None;
    }
    selected
        .subtitle_languages
        .iter()
        .map(|language| language.trim().to_string())
        .find(|language| {
            // MeTube accepts `^[A-Za-z0-9][A-Za-z0-9-]{0,34}$`.
            let mut chars = language.chars();
            matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric())
                && language.len() <= 35
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// A sub-folder name MeTube accepts under `DOWNLOAD_DIR`: the host's basename
/// with path separators and traversal removed. MeTube rejects a folder that
/// escapes its base directory, so the plugin never produces one.
fn folder_for(output_basename: &str) -> String {
    let cleaned: String = output_basename
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '-',
            other => other,
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c == '.' || c == '-' || c.is_whitespace());
    if cleaned.is_empty() {
        "scryer".to_string()
    } else {
        cleaned.to_string()
    }
}

fn row_url(download: &MetubeDownload) -> Option<&str> {
    download
        .url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
}

impl MetubeHistory {
    fn active(&self) -> impl Iterator<Item = &MetubeDownload> {
        self.queue.iter().chain(self.pending.iter())
    }

    fn is_active(&self, url: &str) -> bool {
        self.active().any(|download| row_url(download) == Some(url))
    }

    fn is_done(&self, url: &str) -> bool {
        self.done
            .iter()
            .any(|download| row_url(download) == Some(url))
    }

    /// `done` rows that still describe the item behind their URL. MeTube does
    /// not refuse a URL it already has in `done`; it queues it again under
    /// the same key and only replaces the `done` row when the new attempt
    /// ends. Until then the old row (an earlier failure, or an earlier file
    /// in another folder) must not be reported for the new download.
    fn settled(&self) -> impl Iterator<Item = &MetubeDownload> {
        self.done
            .iter()
            .filter(|download| row_url(download).is_some_and(|url| !self.is_active(url)))
    }
}

/// The key MeTube holds the just-added item under. The submitted URL when a
/// row carries it; otherwise the one row whose yt-dlp `id` is the release's
/// item id (yt-dlp rewrote the URL to its `webpage_url`), active rows first.
/// `None` when neither identifies exactly one item.
fn find_item_key(history: &MetubeHistory, page_url: &str, item_id: &str) -> Option<String> {
    if history.is_active(page_url) || history.is_done(page_url) {
        return Some(page_url.to_string());
    }
    if item_id.is_empty() {
        return None;
    }
    let by_id = |rows: &mut dyn Iterator<Item = &MetubeDownload>| -> Option<String> {
        let mut urls: Vec<&str> = rows
            .filter(|download| download.id.as_deref().map(str::trim) == Some(item_id))
            .filter_map(row_url)
            .collect();
        urls.sort_unstable();
        urls.dedup();
        match urls.as_slice() {
            [url] => Some((*url).to_string()),
            _ => None,
        }
    };
    by_id(&mut history.active()).or_else(|| by_id(&mut history.done.iter()))
}

/// A relative path that stays below its base: not absolute (`/`, `\`, a
/// drive letter) and with no `..` segment.
fn is_contained_relative(path: &str) -> bool {
    let absolute =
        path.starts_with('/') || path.starts_with('\\') || path.as_bytes().get(1) == Some(&b':');
    let traverses = path.split(['/', '\\']).any(|segment| segment == "..");
    !absolute && !traverses
}

/// The item's file relative to `DOWNLOAD_DIR`, when it names a file inside
/// it. MeTube stores `filename` relative to the item's own directory
/// (`DOWNLOAD_DIR/folder`, via `os.path.relpath`), and `folder` separately,
/// so the two are joined here; an empty or missing `folder` is the root.
/// A file outside that directory (a `TEMP_DIR` part file, say) comes back with
/// `..` segments. An absolute or traversing `folder` or `filename` is never
/// reported, because a client-returned path is no permission to touch
/// anything outside the mapped root.
fn safe_relative_filename(download: &MetubeDownload) -> Option<String> {
    let filename = download
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|filename| !filename.is_empty())?;
    if !is_contained_relative(filename) {
        return None;
    }
    let folder = download
        .folder
        .as_deref()
        .map(|folder| folder.trim().trim_end_matches(['/', '\\']))
        .filter(|folder| !folder.is_empty());
    match folder {
        None => Some(filename.to_string()),
        Some(folder) if is_contained_relative(folder) => {
            let filename = filename.trim_start_matches("./");
            Some(format!("{folder}/{filename}"))
        }
        Some(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Listings
// ---------------------------------------------------------------------------

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    respond(list_queue())
}

fn list_queue() -> Result<Vec<PluginDownloadItem>, PluginError> {
    let config = MetubeConfig::from_host();
    let history = fetch_history(&config)?;
    Ok(queue_items(&history, &config))
}

fn queue_items(history: &MetubeHistory, config: &MetubeConfig) -> Vec<PluginDownloadItem> {
    history
        .active()
        .filter_map(|download| download_to_item(download, config))
        .collect()
}

/// Failed downloads live in MeTube's `done` list with `status: "error"`. The
/// PDK bridge merges these into the queue poll, so a failure reaches Scryer
/// without the item having to stay in the queue.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    respond(list_history())
}

fn list_history() -> Result<Vec<PluginDownloadItem>, PluginError> {
    let config = MetubeConfig::from_host();
    let history = fetch_history(&config)?;
    Ok(failed_items(&history, &config))
}

/// `done` rows that failed, plus finished rows whose file cannot be reported
/// (no `filename`, or one outside `DOWNLOAD_DIR`). The latter would otherwise
/// leave the queue and never appear as completed, so Scryer would wait on
/// them forever; they are reported as failed with the reason instead. A row
/// superseded by a fresh attempt at the same URL is skipped (see
/// [`MetubeHistory::settled`]), or the bridge's terminal-state merge would
/// fail the new attempt with the old error.
fn failed_items(history: &MetubeHistory, config: &MetubeConfig) -> Vec<PluginDownloadItem> {
    history
        .settled()
        .filter_map(|download| match download.status.as_deref() {
            Some("error") => download_to_item(download, config),
            Some("finished") if safe_relative_filename(download).is_none() => {
                let mut item = download_to_item(download, config)?;
                item.state = DownloadItemState::Failed;
                item.can_move_files = Some(false);
                item.message = Some(if download.filename.is_some() {
                    "MeTube finished, but reported a file outside its download directory; Scryer will not import it.".to_string()
                } else {
                    "MeTube finished, but reported no file for this download.".to_string()
                });
                Some(item)
            }
            _ => None,
        })
        .collect()
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    respond(list_completed())
}

fn list_completed() -> Result<Vec<PluginCompletedDownload>, PluginError> {
    let config = MetubeConfig::from_host();
    let history = fetch_history(&config)?;
    Ok(completed_downloads(&history, &config))
}

fn completed_downloads(
    history: &MetubeHistory,
    config: &MetubeConfig,
) -> Vec<PluginCompletedDownload> {
    history
        .settled()
        .filter(|download| download.status.as_deref() == Some("finished"))
        .filter_map(|download| download_to_completed(download, config))
        .collect()
}

fn download_to_item(
    download: &MetubeDownload,
    config: &MetubeConfig,
) -> Option<PluginDownloadItem> {
    let url = row_url(download)?;
    let raw_state = download.status.clone().unwrap_or_default();
    let (state, phase) = map_state(&raw_state);
    let progress = download
        .percent
        .filter(|percent| percent.is_finite())
        .map(|percent| percent.clamp(0.0, 100.0).floor() as u8);
    let total_size = download
        .size
        .filter(|size| size.is_finite() && *size > 0.0)
        .map(|size| size as i64);
    let remaining = match (total_size, progress) {
        (Some(total), Some(percent)) => Some(total - total * i64::from(percent) / 100),
        (Some(total), None) => Some(total),
        _ => None,
    };
    let message = match state {
        DownloadItemState::Failed => download
            .msg
            .clone()
            .filter(|message| !message.trim().is_empty())
            .or_else(|| download.error.clone())
            .filter(|message| !message.trim().is_empty()),
        _ => download
            .msg
            .clone()
            .filter(|message| !message.trim().is_empty()),
    };
    let remote_output_path = safe_relative_filename(download)
        .map(|relative| join_remote(&config.download_dir, &relative));

    Some(PluginDownloadItem {
        client_item_id: url.to_string(),
        download_id: None,
        info_hash: None,
        title: download
            .title
            .clone()
            .or_else(|| download.id.clone())
            .unwrap_or_else(|| url.to_string()),
        state,
        message,
        category: None,
        remote_output_path,
        torrent: None,
        http: Some(PluginHttpItem {
            transport_in_use: Some(HttpTransport::ProviderPage),
            phase,
        }),
        total_size_bytes: total_size,
        remaining_size_bytes: remaining,
        eta_seconds: download
            .eta
            .filter(|eta| eta.is_finite() && *eta >= 0.0)
            .map(|eta| eta as i64),
        progress_percent: progress,
        can_move_files: Some(state == DownloadItemState::Completed),
        // MeTube has no seeding obligation; a finished item can go any time.
        can_remove: Some(true),
        removed: None,
        raw_state: Some(raw_state),
        completed_at: None,
    })
}

/// MeTube's status vocabulary (`ytdl.py`): `pending`, `queued`, `scheduled`,
/// `preparing`, `downloading`, `postprocessing`, `finished`, `error`.
/// Cancellation is removal from the queue, not a status.
fn map_state(status: &str) -> (DownloadItemState, Option<HttpDownloadPhase>) {
    match status {
        "pending" | "queued" | "scheduled" => (DownloadItemState::Queued, None),
        "preparing" => (
            DownloadItemState::Downloading,
            Some(HttpDownloadPhase::Resolving),
        ),
        "downloading" => (
            DownloadItemState::Downloading,
            Some(HttpDownloadPhase::Downloading),
        ),
        // yt-dlp's post-processing on a video download is the merge of the
        // separate audio and video tracks.
        "postprocessing" => (
            DownloadItemState::Downloading,
            Some(HttpDownloadPhase::Muxing),
        ),
        "finished" => (DownloadItemState::Completed, None),
        "error" => (DownloadItemState::Failed, None),
        // A status this plugin does not know keeps polling rather than
        // failing the download.
        _ => (DownloadItemState::Downloading, None),
    }
}

fn download_to_completed(
    download: &MetubeDownload,
    config: &MetubeConfig,
) -> Option<PluginCompletedDownload> {
    let url = row_url(download)?;
    let relative = safe_relative_filename(download)?;
    let filename = relative.as_str();
    let path = join_remote(&config.download_dir, filename);
    let dest_dir = parent_dir(&path).unwrap_or_else(|| config.download_dir.clone());
    let name = download
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| file_name(filename).to_string());

    Some(PluginCompletedDownload {
        client_item_id: url.to_string(),
        download_id: None,
        info_hash: None,
        name,
        release_name: None,
        dest_dir,
        category: None,
        output_kind: Some(PluginDownloadOutputKind::File),
        content_paths: vec![path],
        subtitle_paths: Vec::new(),
        size_bytes: download
            .size
            .filter(|size| size.is_finite() && *size > 0.0)
            .map(|size| size as i64),
        // MeTube records no completion time (see `MetubeDownload`).
        completed_at: None,
        parameters: Vec::new(),
    })
}

/// `relative` is the item's path under `DOWNLOAD_DIR` (`folder/filename`, see
/// [`safe_relative_filename`]), so the remote path is the configured
/// directory plus the relative path, joined
/// with a forward slash the way MeTube's Linux container writes it.
fn join_remote(download_dir: &str, relative: &str) -> String {
    let root = download_dir.trim().trim_end_matches('/');
    let relative = relative
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    if root.is_empty() {
        return relative.to_string();
    }
    format!("{root}/{relative}")
}

fn parent_dir(path: &str) -> Option<String> {
    path.rfind('/').map(|index| path[..index].to_string())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Control, status, test
// ---------------------------------------------------------------------------

pub fn scryer_download_control(input: String) -> FnResult<String> {
    respond(control(&input))
}

fn control(input: &str) -> Result<(), PluginError> {
    let request: PluginDownloadClientControlRequest = parse_request(input)?;
    let config = MetubeConfig::from_host();
    match request.action {
        DownloadControlAction::Remove => {
            let history = fetch_history(&config)?;
            let Some(body) = build_delete_body(&request, &history) else {
                // MeTube no longer holds the item: the remove is already done.
                return Ok(());
            };
            let status: MetubeStatus = send_json(&config, "POST", "delete", Some(&body))?;
            if status.is_ok() {
                Ok(())
            } else {
                let message = status.msg.unwrap_or_default();
                Err(detailed_error(
                    PluginErrorCode::Permanent,
                    format!(
                        "MeTube refused to remove the download: {}",
                        truncate(&message)
                    ),
                    message,
                ))
            }
        }
        DownloadControlAction::Pause | DownloadControlAction::Resume => Err(plugin_error(
            PluginErrorCode::Unsupported,
            "MeTube cannot pause or resume a download.",
        )),
        DownloadControlAction::ForceStart => Err(plugin_error(
            PluginErrorCode::Unsupported,
            "MeTube starts every download Scryer adds; there is nothing to force.",
        )),
    }
}

/// `delete` takes the item's URL in `ids`, and the list it names must be the
/// one MeTube holds the item in: `cancel` and `clear` each skip an id that is
/// not in their own list and still answer `ok`. So the list is chosen from
/// `history`, not from `is_history` alone. A failed item is in `done` although
/// Scryer sees it in its queue poll (the bridge merges failed history there),
/// and a removal routed to `queue` would leave it in MeTube for good.
///
/// - Active (`queue` or `pending`): cancelled with `where: "queue"`, unless
///   Scryer asked to clear a history entry, in which case the active row is a
///   fresh attempt at the same URL and is left alone (`None`).
/// - Only in `done`: cleared with `where: "done"`. `delete_files` is `true`
///   only for a history clear Scryer sent with `remove_data`; a queue-side
///   removal that lands on a `done` row sends `false`.
/// - Nowhere: `None`; there is nothing left to remove.
///
/// MeTube's own `DELETE_FILE_ON_TRASHCAN=true` deletes on every clear
/// regardless of `delete_files`; the plugin README documents that setting.
fn build_delete_body(
    request: &PluginDownloadClientControlRequest,
    history: &MetubeHistory,
) -> Option<Value> {
    let id = request.client_item_id.trim();
    if history.is_active(id) {
        if request.is_history {
            return None;
        }
        return Some(json!({
            "ids": [id],
            "where": "queue",
        }));
    }
    if history.is_done(id) {
        return Some(json!({
            "ids": [id],
            "where": "done",
            "delete_files": request.is_history && request.remove_data,
        }));
    }
    None
}

pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let _request: PluginDownloadClientMarkImportedRequest = match parse_request(&input) {
        Ok(request) => request,
        Err(error) => return respond(Err::<(), PluginError>(error)),
    };
    // MeTube has nothing to label; import leaves the history row as it is.
    respond(Ok::<(), PluginError>(()))
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    respond(client_status())
}

fn client_status() -> Result<PluginDownloadClientStatus, PluginError> {
    let config = MetubeConfig::from_host();
    let version = fetch_version(&config)?;
    // Nothing MeTube exposes over HTTP reveals a misconfiguration worth a
    // warning: `DELETE_FILE_ON_TRASHCAN` and `CUSTOM_DIRS` are only pushed
    // over Socket.IO, so the README documents them instead. A refused
    // sub-folder surfaces as an `InvalidConfig` error on the add itself.
    let warnings = Vec::new();
    Ok(PluginDownloadClientStatus {
        version: Some(version),
        is_localhost: Some(is_localhost_url(&config.base_url)),
        remote_output_roots: vec![config.download_dir.clone()],
        // A finished item stays in MeTube's history until it is cleared.
        removes_completed_downloads: Some(false),
        sorting_mode: None,
        warnings,
    })
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    respond(test_connection())
}

/// The version, then MeTube's `formats` catalogue: every `quality`, `codec`
/// and `format` value this plugin can send must be one MeTube offers for a
/// `video` download, or adds would be refused one by one later.
fn test_connection() -> Result<String, PluginError> {
    let config = MetubeConfig::from_host();
    let version = fetch_version(&config)?;
    let catalogue: Value = send_json(&config, "GET", "formats", None).map_err(|error| {
        if error.code == PluginErrorCode::InvalidConfig {
            // `version` answered, so a 404 here is an older MeTube.
            detailed_error(
                PluginErrorCode::InvalidConfig,
                "This MeTube has no download catalogue (`formats`); update MeTube to a current release.",
                error.debug_message.unwrap_or_default(),
            )
        } else {
            error
        }
    })?;
    check_catalogue(&catalogue)?;
    Ok(version)
}

fn option_ids(choice: &Value) -> Vec<&str> {
    choice["options"]
        .as_array()
        .map(|options| {
            options
                .iter()
                .filter_map(|option| option["id"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

/// Checks `format_catalog.public_catalog()`'s shape: `download_type.options[]`
/// with the `video` entry carrying `codec.options[]` and `format.options[]`,
/// each format with its own `quality.options[]`, every option keyed by `id`.
fn check_catalogue(catalogue: &Value) -> Result<(), PluginError> {
    let video = catalogue["download_type"]["options"]
        .as_array()
        .and_then(|types| types.iter().find(|entry| entry["id"] == "video"))
        .ok_or_else(|| {
            plugin_error(
                PluginErrorCode::InvalidConfig,
                "MeTube's download catalogue has no `video` download type this plugin can read.",
            )
        })?;

    let mut missing = Vec::new();
    let codecs = option_ids(&video["codec"]);
    for codec in std::iter::once(&"auto").chain(CODEC_CHOICES) {
        if !codecs.contains(codec) {
            missing.push(format!("codec {codec}"));
        }
    }
    let formats = video["format"]["options"].as_array();
    for format in FORMAT_CHOICES {
        let entry = formats.and_then(|formats| formats.iter().find(|entry| entry["id"] == *format));
        let Some(entry) = entry else {
            missing.push(format!("format {format}"));
            continue;
        };
        let qualities = option_ids(&entry["quality"]);
        let ladder = QUALITY_LADDER.iter().map(u32::to_string);
        for quality in ["best".to_string(), "worst".to_string()]
            .into_iter()
            .chain(ladder)
        {
            if !qualities.contains(&quality.as_str()) {
                missing.push(format!("quality {quality} for format {format}"));
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "MeTube does not offer values this plugin sends: {}. Update MeTube to a current release.",
                missing.join(", ")
            ),
        ))
    }
}

// ---------------------------------------------------------------------------
// Component wiring
// ---------------------------------------------------------------------------

wit_bindgen::generate!({
    world: "scryer:download-client/download-client@1.0.0",
    path: ["../../pdk/scryer-plugin-pdk/wit/host-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/download-client-v1.0.0"],
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

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{PluginHttpPolicy, PluginHttpRelease};

    fn config() -> MetubeConfig {
        MetubeConfig {
            base_url: "http://metube.invalid:8081/".to_string(),
            username: String::new(),
            password: String::new(),
            download_dir: "/downloads".to_string(),
            subfolder_per_download: true,
        }
    }

    fn source(selected: PluginHttpSelection) -> PluginHttpDownloadSource {
        PluginHttpDownloadSource {
            release: PluginHttpRelease {
                provider: "synthetic".to_string(),
                item_id: "clip-1".to_string(),
                page_url: "https://video.invalid/watch?v=clip-1".to_string(),
                ..Default::default()
            },
            selected,
            policy: PluginHttpPolicy {
                output_basename: "Synthetic Show - S01E01-dl-42".to_string(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn base_url_always_ends_with_one_slash() {
        assert_eq!(normalize_base_url("http://m:8081"), "http://m:8081/");
        assert_eq!(normalize_base_url("http://m:8081///"), "http://m:8081/");
        assert_eq!(
            normalize_base_url("m:8081/metube/"),
            "http://m:8081/metube/"
        );
        assert_eq!(normalize_base_url("  "), "http://localhost:8081/");
    }

    #[test]
    fn quality_maps_the_ceiling_onto_the_ladder() {
        assert_eq!(quality_for(None), "best");
        assert_eq!(quality_for(Some(2160)), "2160");
        assert_eq!(quality_for(Some(1080)), "1080");
        // Between rungs rounds down, never up past the profile ceiling.
        assert_eq!(quality_for(Some(900)), "720");
        assert_eq!(quality_for(Some(4320)), "2160");
        assert_eq!(quality_for(Some(144)), "worst");
    }

    #[test]
    fn codec_takes_the_first_preference_metube_offers() {
        assert_eq!(codec_for(&[]), "auto");
        assert_eq!(codec_for(&["xvid".to_string(), "AV1".to_string()]), "av1");
        assert_eq!(codec_for(&["h265".to_string(), "h264".to_string()]), "h265");
        assert_eq!(codec_for(&["prores".to_string()]), "auto");
    }

    #[test]
    fn format_is_mp4_only_when_mp4_leads() {
        assert_eq!(format_for(&[]), "any");
        assert_eq!(format_for(&["MP4".to_string()]), "mp4");
        assert_eq!(format_for(&["mkv".to_string(), "mp4".to_string()]), "any");
    }

    #[test]
    fn folder_never_escapes_the_download_dir() {
        assert_eq!(folder_for("../../etc"), "etc");
        assert_eq!(folder_for("a/b\\c:d"), "a-b-c-d");
        assert_eq!(folder_for(" . "), "scryer");
        assert_eq!(folder_for("Synthetic-dl-42"), "Synthetic-dl-42");
    }

    #[test]
    fn add_body_carries_the_catalogue_mapping_and_never_overrides() {
        let selected = PluginHttpSelection {
            max_height: Some(1080),
            video_codec_preference: vec!["av1".to_string(), "h264".to_string()],
            container_preference: vec!["mkv".to_string()],
            subtitle_languages: vec!["en".to_string()],
            subtitle_mode: HttpSubtitleMode::SidecarPreferred,
            ..Default::default()
        };
        let body = build_add_body(&source(selected), &config());
        assert_eq!(body["url"], "https://video.invalid/watch?v=clip-1");
        assert_eq!(body["download_type"], "video");
        assert_eq!(body["quality"], "1080");
        assert_eq!(body["codec"], "av1");
        assert_eq!(body["format"], "any");
        assert_eq!(body["folder"], "Synthetic Show - S01E01-dl-42");
        assert_eq!(body["playlist_item_limit"], 1);
        assert_eq!(body["auto_start"], true);
        assert_eq!(body["subtitle_language"], "en");
        assert_eq!(body["subtitle_mode"], "prefer_manual");
        assert!(body.get("ytdl_options_overrides").is_none());
        assert!(body.get("ytdl_options_presets").is_none());
    }

    #[test]
    fn add_body_omits_subtitles_and_folder_when_not_wanted() {
        let mut config = config();
        config.subfolder_per_download = false;
        let body = build_add_body(&source(PluginHttpSelection::default()), &config);
        assert_eq!(body["quality"], "best");
        assert_eq!(body["codec"], "auto");
        assert!(body.get("folder").is_none());
        assert!(body.get("subtitle_language").is_none());
    }

    #[test]
    fn states_follow_metube_vocabulary() {
        assert_eq!(map_state("pending").0, DownloadItemState::Queued);
        assert_eq!(map_state("queued").0, DownloadItemState::Queued);
        assert_eq!(
            map_state("preparing"),
            (
                DownloadItemState::Downloading,
                Some(HttpDownloadPhase::Resolving)
            )
        );
        assert_eq!(
            map_state("postprocessing"),
            (
                DownloadItemState::Downloading,
                Some(HttpDownloadPhase::Muxing)
            )
        );
        assert_eq!(map_state("finished").0, DownloadItemState::Completed);
        assert_eq!(map_state("error").0, DownloadItemState::Failed);
        assert_eq!(map_state("something-new").0, DownloadItemState::Downloading);
    }

    #[test]
    fn queue_rows_become_items_keyed_by_url() {
        let row: MetubeDownload = serde_json::from_value(json!({
            "id": "clip-1",
            "title": "Synthetic Episode",
            "url": "https://video.invalid/watch?v=clip-1",
            "status": "downloading",
            "percent": 42.7,
            "eta": 12.5,
            "size": 1000.0,
            "folder": "Synthetic Show - S01E01-dl-42",
            "filename": "Synthetic Episode.mkv"
        }))
        .unwrap();
        let item = download_to_item(&row, &config()).unwrap();
        assert_eq!(item.client_item_id, "https://video.invalid/watch?v=clip-1");
        assert_eq!(item.state, DownloadItemState::Downloading);
        assert_eq!(item.progress_percent, Some(42));
        assert_eq!(item.eta_seconds, Some(12));
        assert_eq!(item.total_size_bytes, Some(1000));
        assert_eq!(item.remaining_size_bytes, Some(580));
        assert_eq!(
            item.remote_output_path.as_deref(),
            Some("/downloads/Synthetic Show - S01E01-dl-42/Synthetic Episode.mkv")
        );
        assert_eq!(
            item.http.unwrap().phase,
            Some(HttpDownloadPhase::Downloading)
        );
        assert_eq!(item.can_move_files, Some(false));
    }

    #[test]
    fn failed_rows_carry_the_error_message() {
        let row: MetubeDownload = serde_json::from_value(json!({
            "url": "https://video.invalid/watch?v=clip-1",
            "status": "error",
            "msg": "",
            "error": "Video unavailable"
        }))
        .unwrap();
        let item = download_to_item(&row, &config()).unwrap();
        assert_eq!(item.state, DownloadItemState::Failed);
        assert_eq!(item.message.as_deref(), Some("Video unavailable"));
    }

    #[test]
    fn rows_without_a_url_are_skipped() {
        let row = MetubeDownload {
            status: Some("queued".to_string()),
            ..Default::default()
        };
        assert!(download_to_item(&row, &config()).is_none());
    }

    #[test]
    fn completed_rows_report_paths_under_the_download_dir() {
        let row: MetubeDownload = serde_json::from_value(json!({
            "title": "Synthetic Episode",
            "url": "https://video.invalid/watch?v=clip-1",
            "status": "finished",
            "size": 123456,
            "timestamp": 1760000000.25,
            "folder": "Synthetic Show - S01E01-dl-42",
            "filename": "Synthetic Episode.mkv"
        }))
        .unwrap();
        let completed = download_to_completed(&row, &config()).unwrap();
        assert_eq!(
            completed.client_item_id,
            "https://video.invalid/watch?v=clip-1"
        );
        assert_eq!(
            completed.dest_dir,
            "/downloads/Synthetic Show - S01E01-dl-42"
        );
        assert_eq!(
            completed.content_paths,
            vec!["/downloads/Synthetic Show - S01E01-dl-42/Synthetic Episode.mkv"]
        );
        assert_eq!(completed.output_kind, Some(PluginDownloadOutputKind::File));
        assert_eq!(completed.size_bytes, Some(123456));
        // MeTube's `timestamp` is `time.time_ns()` at creation, not a
        // completion time in seconds; it must never be reported as one.
        assert_eq!(completed.completed_at, None);
    }

    fn finished(folder: Option<&str>, filename: &str) -> MetubeDownload {
        MetubeDownload {
            url: Some(CLIP.to_string()),
            status: Some("finished".to_string()),
            folder: folder.map(str::to_string),
            filename: Some(filename.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn filename_is_joined_under_the_items_folder() {
        // MeTube stores `filename` relative to `DOWNLOAD_DIR/folder`.
        let row = finished(
            Some("Synthetic Show - S01E01-dl-42"),
            "Synthetic Episode.webm",
        );
        let completed = download_to_completed(&row, &config()).unwrap();
        assert_eq!(
            completed.content_paths,
            vec!["/downloads/Synthetic Show - S01E01-dl-42/Synthetic Episode.webm"]
        );
        assert_eq!(
            completed.dest_dir,
            "/downloads/Synthetic Show - S01E01-dl-42"
        );
        assert_eq!(
            download_to_item(&row, &config())
                .unwrap()
                .remote_output_path
                .as_deref(),
            Some("/downloads/Synthetic Show - S01E01-dl-42/Synthetic Episode.webm")
        );

        // No folder, or an empty one, is the root.
        for folder in [None, Some(""), Some("  ")] {
            let completed =
                download_to_completed(&finished(folder, "Synthetic Episode.webm"), &config())
                    .unwrap();
            assert_eq!(
                completed.content_paths,
                vec!["/downloads/Synthetic Episode.webm"],
                "{folder:?}"
            );
            assert_eq!(completed.dest_dir, "/downloads");
        }

        // A trailing separator on the folder does not double the slash.
        let completed =
            download_to_completed(&finished(Some("Sub/"), "x.webm"), &config()).unwrap();
        assert_eq!(completed.content_paths, vec!["/downloads/Sub/x.webm"]);
    }

    #[test]
    fn a_traversing_or_absolute_folder_is_never_reported() {
        for folder in [
            "../outside",
            "Sub/../../etc",
            "/etc",
            "\\\\server\\share",
            "C:\\x",
        ] {
            let row = finished(Some(folder), "Synthetic Episode.webm");
            assert!(
                download_to_completed(&row, &config()).is_none(),
                "{folder} must not be offered for import"
            );
            assert_eq!(
                download_to_item(&row, &config())
                    .unwrap()
                    .remote_output_path,
                None,
                "{folder} must not be reported as an output path"
            );
            let history = MetubeHistory {
                done: vec![row],
                ..Default::default()
            };
            assert_eq!(
                failed_items(&history, &config())[0].state,
                DownloadItemState::Failed,
                "{folder}: a finished row with an unusable path is reported failed"
            );
        }
    }

    #[test]
    fn traversing_or_absolute_filenames_are_never_reported() {
        for filename in [
            "../temp/Synthetic Episode.mkv.part",
            "sub/../../etc/passwd",
            "/etc/passwd",
            "\\\\server\\share\\x.mkv",
            "C:\\downloads\\x.mkv",
            "sub\\..\\..\\x.mkv",
        ] {
            let row: MetubeDownload = serde_json::from_value(json!({
                "url": "https://video.invalid/watch?v=clip-1",
                "status": "finished",
                "filename": filename
            }))
            .unwrap();
            assert!(
                download_to_completed(&row, &config()).is_none(),
                "{filename} must not be offered for import"
            );
            assert_eq!(
                download_to_item(&row, &config())
                    .unwrap()
                    .remote_output_path,
                None,
                "{filename} must not be reported as an output path"
            );
        }
    }

    #[test]
    fn a_finished_row_without_a_usable_file_is_reported_failed_not_dropped() {
        let history: MetubeHistory = serde_json::from_value(json!({
            "done": [
                { "url": "https://video.invalid/watch?v=clip-1", "status": "finished",
                  "filename": "../outside/x.mkv" },
                { "url": "https://video.invalid/watch?v=clip-2", "status": "finished" },
                { "url": "https://video.invalid/watch?v=clip-3", "status": "finished",
                  "filename": "inside/x.mkv" }
            ]
        }))
        .unwrap();
        let failed = failed_items(&history, &config());
        let ids: Vec<&str> = failed
            .iter()
            .map(|item| item.client_item_id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "https://video.invalid/watch?v=clip-1",
                "https://video.invalid/watch?v=clip-2"
            ]
        );
        assert!(
            failed
                .iter()
                .all(|item| item.state == DownloadItemState::Failed
                    && item.can_move_files == Some(false)
                    && item.message.is_some())
        );
        let completed = completed_downloads(&history, &config());
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].client_item_id,
            "https://video.invalid/watch?v=clip-3"
        );
    }

    #[test]
    fn a_fresh_attempt_supersedes_the_old_done_row_for_the_same_url() {
        // MeTube re-queues a URL it already has in `done` under the same key.
        let history: MetubeHistory = serde_json::from_value(json!({
            "done": [
                { "url": "https://video.invalid/watch?v=clip-1", "status": "error",
                  "msg": "earlier failure" },
                { "url": "https://video.invalid/watch?v=clip-2", "status": "finished",
                  "filename": "Old Folder/old.mkv" }
            ],
            "queue": [
                { "url": "https://video.invalid/watch?v=clip-1", "status": "downloading" },
                { "url": "https://video.invalid/watch?v=clip-2", "status": "preparing" }
            ]
        }))
        .unwrap();
        assert!(
            failed_items(&history, &config()).is_empty(),
            "the old failure must not fail the new attempt"
        );
        assert!(
            completed_downloads(&history, &config()).is_empty(),
            "the old file must not be imported for the new attempt"
        );
        assert_eq!(queue_items(&history, &config()).len(), 2);
    }

    #[test]
    fn completed_rows_without_a_filename_are_not_offered() {
        let row: MetubeDownload = serde_json::from_value(json!({
            "url": "https://video.invalid/watch?v=clip-1",
            "status": "finished",
            "filename": ""
        }))
        .unwrap();
        assert!(download_to_completed(&row, &config()).is_none());
    }

    #[test]
    fn remote_paths_join_with_one_slash() {
        assert_eq!(join_remote("/downloads/", "a/b.mkv"), "/downloads/a/b.mkv");
        assert_eq!(join_remote("/downloads", "./a/b.mkv"), "/downloads/a/b.mkv");
        assert_eq!(join_remote("", "a/b.mkv"), "a/b.mkv");
    }

    fn remove(id: &str, is_history: bool, remove_data: bool) -> PluginDownloadClientControlRequest {
        PluginDownloadClientControlRequest {
            action: DownloadControlAction::Remove,
            client_item_id: id.to_string(),
            remove_data,
            is_history,
        }
    }

    fn history_with(active: &[&str], done: &[&str]) -> MetubeHistory {
        let row = |url: &&str, status: &str| MetubeDownload {
            url: Some(url.to_string()),
            status: Some(status.to_string()),
            ..Default::default()
        };
        MetubeHistory {
            queue: active.iter().map(|url| row(url, "downloading")).collect(),
            done: done.iter().map(|url| row(url, "finished")).collect(),
            pending: Vec::new(),
        }
    }

    const CLIP: &str = "https://video.invalid/watch?v=clip-1";

    #[test]
    fn an_active_item_is_cancelled_and_never_carries_delete_files() {
        let history = history_with(&[CLIP], &[]);
        for remove_data in [false, true] {
            let body = build_delete_body(&remove(CLIP, false, remove_data), &history).unwrap();
            assert_eq!(body["where"], "queue");
            assert_eq!(body["ids"], json!([CLIP]));
            assert!(body.get("delete_files").is_none());
        }
    }

    #[test]
    fn a_history_clear_sends_delete_files_only_with_remove_data() {
        let history = history_with(&[], &[CLIP]);
        let body = build_delete_body(&remove(CLIP, true, false), &history).unwrap();
        assert_eq!(body["where"], "done");
        assert_eq!(body["delete_files"], false);
        let body = build_delete_body(&remove(CLIP, true, true), &history).unwrap();
        assert_eq!(body["delete_files"], true);
    }

    #[test]
    fn a_queue_side_remove_of_a_done_row_clears_it_without_deleting_files() {
        // A failed item reaches Scryer through the queue poll, but MeTube
        // holds it in `done`; `where: "queue"` would be a silent no-op.
        let history = history_with(&[], &[CLIP]);
        let body = build_delete_body(&remove(CLIP, false, true), &history).unwrap();
        assert_eq!(body["where"], "done");
        assert_eq!(body["delete_files"], false);
    }

    #[test]
    fn a_history_clear_never_cancels_a_fresh_attempt_at_the_same_url() {
        let history = history_with(&[CLIP], &[CLIP]);
        assert!(build_delete_body(&remove(CLIP, true, true), &history).is_none());
    }

    #[test]
    fn removing_an_item_metube_no_longer_has_sends_nothing() {
        let history = history_with(&[], &[]);
        assert!(build_delete_body(&remove(CLIP, false, false), &history).is_none());
        assert!(build_delete_body(&remove(CLIP, true, true), &history).is_none());
    }

    #[test]
    fn item_key_is_the_submitted_url_when_metube_kept_it() {
        let history = history_with(&[CLIP], &[]);
        assert_eq!(
            find_item_key(&history, CLIP, "clip-1").as_deref(),
            Some(CLIP)
        );
        let history = history_with(&[], &[CLIP]);
        assert_eq!(
            find_item_key(&history, CLIP, "clip-1").as_deref(),
            Some(CLIP)
        );
    }

    #[test]
    fn item_key_follows_a_rewritten_url_by_item_id() {
        // yt-dlp keyed the item by its `webpage_url`, not the submitted URL.
        let history: MetubeHistory = serde_json::from_value(json!({
            "queue": [
                { "id": "clip-1", "url": "https://video.invalid/watch?v=clip-1&canonical=1",
                  "status": "preparing" },
                { "id": "clip-9", "url": "https://video.invalid/watch?v=clip-9",
                  "status": "queued" }
            ]
        }))
        .unwrap();
        assert_eq!(
            find_item_key(&history, "https://short.invalid/clip-1", "clip-1").as_deref(),
            Some("https://video.invalid/watch?v=clip-1&canonical=1")
        );
        // Not found, and no id to go on: nothing to reconcile against.
        assert_eq!(
            find_item_key(&history, "https://short.invalid/clip-2", "clip-2"),
            None
        );
        assert_eq!(
            find_item_key(&history, "https://short.invalid/clip-2", ""),
            None
        );
    }

    #[test]
    fn item_key_refuses_an_ambiguous_item_id() {
        let history: MetubeHistory = serde_json::from_value(json!({
            "queue": [
                { "id": "clip-1", "url": "https://a.invalid/clip-1", "status": "queued" },
                { "id": "clip-1", "url": "https://b.invalid/clip-1", "status": "queued" }
            ]
        }))
        .unwrap();
        assert_eq!(
            find_item_key(&history, "https://c.invalid/clip-1", "clip-1"),
            None
        );
    }

    #[test]
    fn a_host_timeout_is_recognised_from_the_service_error_debug_message() {
        // The command host's public message is generic; the cause is only in
        // `debug_message`, which `Display` drops.
        let error: scryer_plugin_pdk::Error =
            scryer_plugin_pdk::host::HostCallError::Service(PluginError {
                code: PluginErrorCode::Temporary,
                public_message: "command plugin host service failed".to_string(),
                debug_message: Some("timeout".to_string()),
                retry_after_seconds: Some(0),
                details: None,
            })
            .into();
        let classified = classify_transport_error(&transport_error_detail(&error));
        assert!(is_timeout(&classified), "{classified:?}");

        let error: scryer_plugin_pdk::Error =
            scryer_plugin_pdk::host::HostCallError::Service(PluginError {
                code: PluginErrorCode::Temporary,
                public_message: "command plugin host service failed".to_string(),
                debug_message: Some("connection refused".to_string()),
                retry_after_seconds: Some(0),
                details: None,
            })
            .into();
        let classified = classify_transport_error(&transport_error_detail(&error));
        assert!(!is_timeout(&classified));
        assert_eq!(classified.code, PluginErrorCode::UpstreamUnavailable);
    }

    #[test]
    fn add_refusals_are_classified() {
        assert_eq!(
            add_refused(Some("ERROR: Video unavailable".to_string())).code,
            PluginErrorCode::Permanent
        );
        assert_eq!(
            add_refused(Some("HTTP Error 429: Too Many Requests".to_string())).code,
            PluginErrorCode::Temporary
        );
        assert_eq!(
            add_refused(Some(
                "A folder for the download was specified but CUSTOM_DIRS is not true in the configuration."
                    .to_string()
            ))
            .code,
            PluginErrorCode::InvalidConfig
        );
        let silent = add_refused(Some("  ".to_string()));
        assert_eq!(silent.code, PluginErrorCode::Permanent);
        assert!(silent.public_message.contains("without a message"));
    }

    #[test]
    fn http_statuses_map_to_error_codes() {
        assert_eq!(http_status_error(401, "").code, PluginErrorCode::AuthFailed);
        assert_eq!(
            http_status_error(404, "").code,
            PluginErrorCode::InvalidConfig
        );
        assert_eq!(http_status_error(429, "").code, PluginErrorCode::Temporary);
        assert_eq!(
            http_status_error(502, "").code,
            PluginErrorCode::UpstreamUnavailable
        );
        assert_eq!(http_status_error(400, "").code, PluginErrorCode::Permanent);
    }

    fn catalogue(video_codecs: &[&str], formats: &[&str], qualities: &[&str]) -> Value {
        let ids = |values: &[&str]| -> Vec<Value> {
            values
                .iter()
                .map(|id| json!({ "id": id, "text": id }))
                .collect()
        };
        json!({
            "download_type": {
                "default": "video",
                "options": [
                    { "id": "audio", "text": "Audio", "format": { "options": [] } },
                    {
                        "id": "video",
                        "text": "Video",
                        "codec": { "default": "auto", "options": ids(video_codecs) },
                        "format": {
                            "default": "any",
                            "options": formats.iter().map(|format| json!({
                                "id": format,
                                "text": format,
                                "quality": { "default": "best", "options": ids(qualities) }
                            })).collect::<Vec<_>>()
                        }
                    }
                ]
            }
        })
    }

    const ALL_QUALITIES: &[&str] = &[
        "best", "2160", "1440", "1080", "720", "480", "360", "240", "worst",
    ];

    #[test]
    fn the_current_catalogue_offers_every_value_the_plugin_sends() {
        let current = catalogue(
            &["auto", "h264", "h265", "av1", "vp9"],
            &["any", "mp4", "ios"],
            ALL_QUALITIES,
        );
        assert!(check_catalogue(&current).is_ok());
    }

    #[test]
    fn a_catalogue_missing_a_value_fails_the_connection_test() {
        let no_vp9 = catalogue(
            &["auto", "h264", "h265", "av1"],
            &["any", "mp4"],
            ALL_QUALITIES,
        );
        let error = check_catalogue(&no_vp9).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("codec vp9"));

        let no_mp4 = catalogue(
            &["auto", "h264", "h265", "av1", "vp9"],
            &["any"],
            ALL_QUALITIES,
        );
        assert!(
            check_catalogue(&no_mp4)
                .unwrap_err()
                .public_message
                .contains("format mp4")
        );

        let no_worst = catalogue(
            &["auto", "h264", "h265", "av1", "vp9"],
            &["any", "mp4"],
            &ALL_QUALITIES[..8],
        );
        assert!(
            check_catalogue(&no_worst)
                .unwrap_err()
                .public_message
                .contains("quality worst")
        );

        assert_eq!(
            check_catalogue(&json!({})).unwrap_err().code,
            PluginErrorCode::InvalidConfig
        );
    }

    #[test]
    fn numeric_fields_tolerate_nulls_negatives_and_overflow() {
        let row: MetubeDownload = serde_json::from_value(json!({
            "url": CLIP,
            "status": "downloading",
            "percent": 250.0,
            "eta": -5.0,
            "size": -1.0,
            "speed": null
        }))
        .unwrap();
        let item = download_to_item(&row, &config()).unwrap();
        assert_eq!(item.progress_percent, Some(100));
        assert_eq!(item.eta_seconds, None);
        assert_eq!(item.total_size_bytes, None);
        assert_eq!(item.remaining_size_bytes, None);

        let row: MetubeDownload = serde_json::from_value(json!({
            "url": CLIP,
            "status": "queued",
            "percent": null,
            "eta": null,
            "size": null
        }))
        .unwrap();
        let item = download_to_item(&row, &config()).unwrap();
        assert_eq!(item.progress_percent, None);
        assert_eq!(item.eta_seconds, None);

        let row: MetubeDownload = serde_json::from_value(json!({
            "url": CLIP,
            "status": "downloading",
            "percent": -3.5
        }))
        .unwrap();
        assert_eq!(
            download_to_item(&row, &config()).unwrap().progress_percent,
            Some(0)
        );
    }

    #[test]
    fn status_replies_are_read_leniently() {
        let ok: MetubeStatus = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        assert!(ok.is_ok());
        let already: MetubeStatus =
            serde_json::from_str(r#"{"status":"ok","msg":"Already in queue: x"}"#).unwrap();
        assert!(already.is_ok());
        let error: MetubeStatus =
            serde_json::from_str(r#"{"status":"error","msg":"bad"}"#).unwrap();
        assert!(!error.is_ok());
    }
}
