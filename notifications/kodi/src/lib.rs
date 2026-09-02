//! Kodi GUI notifications and video-library maintenance over JSON-RPC, as a
//! WASI Preview 2 component.
//!
//! # Why this plugin is called `kodi`
//!
//! XBMC was renamed Kodi in 2014. Sonarr's own display name has been "Kodi"
//! since then (`Xbmc.cs:132`), and only its namespace still says XBMC. The
//! channel therefore ships as `kodi` — directory, crate, `plugin_id`,
//! `provider_type` and display name — with `provider_aliases = ["xbmc"]` so a
//! channel an operator configured under the old id keeps resolving. Every
//! configuration key is unchanged. This follows the `mediabrowser` → `emby`
//! precedent (`git log -1 9ef0a0d`).
//!
//! # What the June port got wrong
//!
//! * **It could never reach a Kodi host.** Scryer builds a plugin's HTTP
//!   allowlist from its descriptor's `allowed_hosts` *unioned with the hosts of
//!   configuration values that parse as URLs*
//!   (`crates/scryer-plugins/src/loader.rs:3143`, `allowed_hosts_for_descriptor`
//!   → `host_from_url`), and an empty allowlist denies every request
//!   (`plugin_http_host.rs:772-797`, "HTTP request to … is not allowed"). The
//!   port declared no static host and addressed Kodi with a bare `host` plus a
//!   `port`, which never parses as a URL — so the allowlist was empty and every
//!   request was refused before it left the sandbox. `server_url` (a
//!   `ConfigFieldRole::ConnectionUrl` field, the same fix the `signal` sibling
//!   carries) is now the primary address; `host`/`port`/`use_ssl` remain as a
//!   legacy fallback that still resolves, warns, and names the URL to paste.
//! * **It embedded an icon that does not exist.**
//!   `…/scryer/main/apps/scryer-web/public/icons/icon-512.png` is a 404, so
//!   every toast asked Kodi to fetch a missing image. `GUI.ShowNotification`'s
//!   `image` parameter is either one of the enum values `info`/`warning`/`error`
//!   or an arbitrary path, so the severity the dispatcher already stamps
//!   (`dispatcher.rs:895`) now selects Kodi's own icon. A poster is sent only
//!   when the operator opts in *and* the event carries one.
//! * **Its `download` arm treated a failure as a success.**
//!   `NotificationEventType::Download` only ever carries a *failed* download:
//!   the dispatcher maps `DownloadFailed` onto it (`dispatcher.rs:418-447`) with
//!   the summary "Download failed: …". The port headed that toast
//!   "Scryer - Downloaded" and ran a library scan for it.
//! * **It never notified on Test unless GUI notifications were enabled**, so a
//!   library-only channel reported success without touching Kodi at all.
//!   Sonarr's `XbmcService.Test` (`XbmcService.cs:127-140`) always notifies.
//! * **Every failure was one opaque string.** `json_rpc_value` folded HTTP
//!   statuses, transport failures, non-JSON bodies and JSON-RPC errors into
//!   `{error:{message}}` and then into a delivery failure, so a wrong password
//!   and an unreachable host were indistinguishable.
//! * **It only knew about series.** Scryer titles carry a `facet`; a movie was
//!   looked up in `VideoLibrary.GetTvShows` and never found, so every movie
//!   event scanned the entire library.
//!
//! # Upstream reference
//!
//! Kodi's JSON-RPC service description is the authority, read 2026-09-02 from
//! `xbmc/interfaces/json-rpc/schema/{methods,types,version}.json` at the
//! `Krypton`, `Leia`, `Matrix`, `Nexus`, `Omega` and `master` branches of
//! <https://github.com/xbmc/xbmc>:
//!
//! * `GUI.ShowNotification(title, message, image, displaytime)` — `image` is
//!   either the enum `info`/`warning`/`error` or a free string; `displaytime` is
//!   an integer in milliseconds with **minimum 1500** and default 5000. Byte for
//!   byte identical from Krypton to master.
//! * `VideoLibrary.Scan(directory = "", showdialogs = true)` — identical from
//!   Krypton to master.
//! * `VideoLibrary.Clean(showdialogs = true, content = "video", directory = "")`
//!   — `showdialogs` only in Krypton (JSON-RPC 8); `content`
//!   (`video`/`movies`/`tvshows`/`musicvideos`) added in Leia (10.3.0);
//!   `directory` added in Matrix (12.4.0). Both are therefore version-gated.
//! * `VideoLibrary.GetTVShows` / `VideoLibrary.GetMovies` — `properties` accept
//!   `file`, `imdbnumber` and `uniqueid` (a `Media.UniqueID` string map) as far
//!   back as Krypton; `Video.Details.TVShow.file` is the show *folder*, while
//!   `Video.Details.Movie.file` is the movie *file*.
//! * `Player.GetActivePlayers` → `[{playerid, type, playertype}]` with
//!   `Player.Type` one of `video`/`audio`/`picture`.
//! * `JSONRPC.Version` → `{version: {major, minor, patch}}` (an integer in
//!   pre-Frodo builds, which is still tolerated here).
//! * JSON-RPC version by release: Kodi 17 Krypton 8.0.0, 18 Leia 10.3.0,
//!   19 Matrix 12.4.0, 20 Nexus 13.0.0, 21 Omega 13.5.0, 22 Piers (beta)
//!   13.200.0.
//!
//! The transport is `POST {server_url}/jsonrpc` with optional HTTP Basic
//! credentials — Kodi's web server ("Settings → Services → Control → Allow
//! remote control via HTTP"), not a Kodi user account.
//!
//! # Why the delivery path is local rather than `notify_common::send_json`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")` and treats every 2xx as a delivery. Neither holds here: a
//! JSON-RPC error arrives inside a **200**, a 401 is the web server's
//! credentials, a 404 is `url_base`, and an HTML body means the web *interface*
//! answered instead of the JSON-RPC endpoint.

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, NotificationMediaUpdateType,
    NotificationSeverity, PluginNotificationRequest as Request, PluginNotificationTargetResult,
    PluginNotificationTitle, current_sdk_constraint,
};
use serde_json::{Map, Value, json};

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:notification/notification@1.0.0",
    // Two packages, two paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/notification-v1.0.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_notification_component_main!(
    descriptor = build_descriptor,
    handler = handle_notification_command,
);

/// A predicate over one library entry, used by the id/label match chains.
type Matcher = Box<dyn Fn(&Value) -> bool>;
/// Reads one identifying string out of a library entry.
type Getter = Box<dyn Fn(&Value) -> Option<String>>;

const PROVIDER_TYPE: &str = "kodi";
/// The id this channel shipped under until the rename. Kept as a descriptor
/// alias so an existing `xbmc` channel still resolves to this plugin.
const LEGACY_PROVIDER_TYPE: &str = "xbmc";
const USER_AGENT: &str = concat!("scryer-kodi-plugin/", env!("CARGO_PKG_VERSION"));

/// `XbmcSettings` constructor defaults (`XbmcSettings.cs:24-29`).
const DEFAULT_PORT: i64 = 8080;
const DEFAULT_URL_BASE: &str = "/jsonrpc";
const DEFAULT_DISPLAY_TIME_SECONDS: i64 = 5;

/// `RuleFor(c => c.DisplayTime).GreaterThanOrEqualTo(2)` (`XbmcSettings.cs:15`).
const MIN_DISPLAY_TIME_SECONDS: i64 = 2;

/// `GUI.ShowNotification`'s own `displaytime` minimum. Kodi rejects anything
/// below it with `Invalid params`, so the configured value is clamped rather
/// than turned into a failed notification.
const KODI_MIN_DISPLAY_TIME_MS: i64 = 1500;

/// A Kodi toast is two or three clipped lines over the video. Anything longer
/// is not shown, so it is truncated here — visibly, and with a warning — rather
/// than being silently cut by the skin.
const MAX_MESSAGE_CHARS: usize = 256;

/// The bound on upstream text quoted back into `public_message`: a web server
/// that answers with a whole HTML page must not turn one failed notification
/// into a wall of markup.
const MAX_QUOTED_ERROR: usize = 300;

/// `uniqueid` on `Video.Fields.TVShow`/`Video.Fields.Movie` — Kodi 17 Krypton.
const JSONRPC_UNIQUEID_MAJOR: i64 = 8;
/// `VideoLibrary.Clean`'s `content` parameter — Kodi 18 Leia.
const JSONRPC_CLEAN_CONTENT_MAJOR: i64 = 10;
/// `VideoLibrary.Clean`'s `directory` parameter — Kodi 19 Matrix.
const JSONRPC_CLEAN_DIRECTORY_MAJOR: i64 = 12;

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Kodi".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            // The rename's whole compatibility story: a channel stored as
            // `xbmc` resolves to this plugin.
            provider_aliases: vec![LEGACY_PROVIDER_TYPE.to_string()],
            // Self-hosted: there is no vendor endpoint to prefill and no host
            // set to allowlist. `server_url` is the only origin this channel
            // ever reaches, and it is what the loader turns into the allowlist.
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                // A Kodi toast is plain text; the skin renders no markup.
                supports_rich_text: false,
                // `GUI.ShowNotification`'s `image` accepts a path or URL, and
                // the poster is sent when the operator opts in.
                supports_images: true,
                supports_test: true,
                // Sonarr batches per host through `MediaServerUpdateQueue` so a
                // season import is one scan; Scryer's core has no equivalent
                // for a plugin channel (`is_media_server_notification_provider`
                // covers only jellyfin/plex/emby, and no batch ever reaches a
                // `PluginNotificationCommand`), so declaring either of these
                // would be a lie. See the report's out-of-fence findings.
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![
                    NotificationDeliveryMode::Chat,
                    NotificationDeliveryMode::MediaServerUpdate,
                ],
                payload_formats: vec![NotificationPayloadFormat::PlainText],
                supported_events: general_notification_events(),
                // Upgrades, delete-for-upgrade and health warnings all render
                // and act differently here, so all three core filters are
                // meaningful for this channel.
                event_options: NotificationEventOptions {
                    supports_upgrade_filter: true,
                    supports_delete_for_upgrade_filter: true,
                    supports_health_warning_filter: true,
                },
            },
            config_fields: config_fields(),
        }),
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "server_url",
            "Server URL",
            false,
            None,
            Some(
                "Kodi's web server, for example http://kodi.local:8080. Scryer builds this channel's network allowlist from configuration values that are URLs, so this is the setting that makes Kodi reachable. Enable it in Kodi under Settings → Services → Control → Allow remote control via HTTP.",
            ),
        ),
        // Sonarr's connection settings. Kept because config keys are a public
        // contract; demoted to "legacy" because a bare host is not a URL and
        // therefore never reaches the loader's allowlist.
        field(
            "host",
            "Host (legacy)",
            ConfigFieldType::String,
            false,
            None,
            Some("Superseded by Server URL. Used only when Server URL is empty."),
        ),
        field(
            "port",
            "Port (legacy)",
            ConfigFieldType::Number,
            false,
            Some("8080"),
            Some("Superseded by Server URL. Used only when Server URL is empty."),
        ),
        field(
            "use_ssl",
            "Use SSL (legacy)",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Superseded by Server URL. Used only when Server URL is empty."),
        ),
        field(
            "url_base",
            "URL Base",
            ConfigFieldType::String,
            false,
            Some(DEFAULT_URL_BASE),
            Some(
                "Path of Kodi's JSON-RPC endpoint. Used with Server URL only when that URL has no path of its own.",
            ),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Kodi web-server user, when 'Require authentication' is enabled. This is not a Kodi profile.",
            ),
        ),
        field(
            "password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            Some("Kodi web-server password."),
        ),
        field(
            "display_time",
            "Display Time",
            ConfigFieldType::Number,
            false,
            Some("5"),
            Some("Seconds the on-screen notification stays visible. Kodi's minimum is 2."),
        ),
        field(
            "notify",
            "GUI Notification",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Show an on-screen notification in Kodi. A connection test always shows one so the operator can see it worked.",
            ),
        ),
        field(
            "notification_poster",
            "Show Poster",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Use the title's poster as the notification icon when the event carries one. Kodi downloads it, so leave this off if Kodi has no internet access. Otherwise the icon is Kodi's own info/warning/error badge.",
            ),
        ),
        field(
            "update_library",
            "Update Library",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Run VideoLibrary.Scan after media changes, scoped to the title's own folder in Kodi when it can be found.",
            ),
        ),
        field(
            "clean_library",
            "Clean Library",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Run VideoLibrary.Clean after events that removed or replaced files."),
        ),
        field(
            "always_update",
            "Always Update",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Update or clean the Kodi library even while video is playing."),
        ),
        field(
            "show_dialogs",
            "Show Progress Dialog",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Let Kodi put its scan/clean progress dialog on screen. Kodi's own default is on; this channel turns it off so an automated scan does not interrupt whoever is watching.",
            ),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

/// `JSONRPC.Version`'s `{major, minor, patch}`.
///
/// Only the major matters for the two gates below, but the whole triple is
/// reported at Test time so the operator can see which Kodi answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JsonRpcVersion {
    major: i64,
    minor: i64,
    patch: i64,
}

impl JsonRpcVersion {
    /// `uniqueid` on `Video.Fields.TVShow`/`Movie`. Below this the only id Kodi
    /// exposes is `imdbnumber`, which is what Sonarr reads.
    fn supports_uniqueid(self) -> bool {
        self.major >= JSONRPC_UNIQUEID_MAJOR
    }

    /// `VideoLibrary.Clean`'s `content`.
    fn supports_clean_content(self) -> bool {
        self.major >= JSONRPC_CLEAN_CONTENT_MAJOR
    }

    /// `VideoLibrary.Clean`'s `directory`.
    fn supports_clean_directory(self) -> bool {
        self.major >= JSONRPC_CLEAN_DIRECTORY_MAJOR
    }
}

impl std::fmt::Display for JsonRpcVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// `VersionResult` (`Model/VersionResult.cs`) reads `result.version` as a
/// `Dictionary<string, int>`, which only covers the object form. Pre-Frodo
/// builds answered with a bare integer, and tolerating that costs one arm.
fn parse_jsonrpc_version(result: &Value) -> Option<JsonRpcVersion> {
    let version = result.get("version")?;
    if let Some(major) = version.as_i64() {
        return Some(JsonRpcVersion {
            major,
            minor: 0,
            patch: 0,
        });
    }
    Some(JsonRpcVersion {
        major: version.get("major").and_then(Value::as_i64)?,
        minor: version.get("minor").and_then(Value::as_i64).unwrap_or(0),
        patch: version.get("patch").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// What a Test should tell the operator about the Kodi that answered.
fn version_warnings(version: JsonRpcVersion) -> Vec<String> {
    let mut warnings = vec![format!("Kodi answered with JSON-RPC version {version}")];
    if !version.supports_uniqueid() {
        warnings.push(format!(
            "this Kodi predates JSON-RPC {JSONRPC_UNIQUEID_MAJOR} (Kodi 17 Krypton), so library lookups fall back to the imdbnumber field and may match the wrong item"
        ));
    }
    if !version.supports_clean_content() {
        warnings.push(format!(
            "this Kodi predates JSON-RPC {JSONRPC_CLEAN_CONTENT_MAJOR} (Kodi 18 Leia), so a library clean cannot be limited to movies or tv shows"
        ));
    } else if !version.supports_clean_directory() {
        warnings.push(format!(
            "this Kodi predates JSON-RPC {JSONRPC_CLEAN_DIRECTORY_MAJOR} (Kodi 19 Matrix), so a library clean cannot be limited to one folder"
        ));
    }
    warnings
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Everything the planner, the renderer and the sender need, resolved and
/// validated once per send so every builder below is a pure function of
/// `(request, settings)` and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    /// Absolute JSON-RPC endpoint, path included.
    endpoint: String,
    /// Whether `endpoint` was assembled from the legacy `host`/`port`/`use_ssl`
    /// keys rather than read from `server_url`.
    legacy_connection: bool,
    /// A ready `Authorization` header value, or `None`.
    auth: Option<String>,
    notify: bool,
    notification_poster: bool,
    display_time_ms: i64,
    update_library: bool,
    clean_library: bool,
    always_update: bool,
    show_dialogs: bool,
}

impl Settings {
    /// `strict` is the Test-time posture, mirroring Sonarr's split between its
    /// settings validator (checked when the form is saved) and its proxy (which
    /// checks nothing on a live send). Rules Kodi itself enforces are errors
    /// either way; a rule that is only *probably* wrong — a display time below
    /// Sonarr's floor but above Kodi's — is refused at Test time and degraded to
    /// a clamped value plus a warning on a live send, because a guess about a
    /// setting is never worth losing a notification over.
    fn from_config(strict: bool) -> Result<(Self, Vec<String>), PluginError> {
        let mut warnings = Vec::new();

        let (endpoint, legacy_connection) = resolve_endpoint(
            config_value("server_url").as_deref(),
            config_value("host").as_deref(),
            config_value("port").as_deref(),
            config_bool("use_ssl"),
            config_value("url_base").as_deref(),
            &mut warnings,
        )?;

        let auth = resolve_auth(
            config_value("username").as_deref(),
            config_value("password").as_deref(),
        );

        let display_time_ms = resolve_display_time_ms(
            config_value("display_time").as_deref(),
            strict,
            &mut warnings,
        )?;

        Ok((
            Self {
                endpoint,
                legacy_connection,
                auth,
                notify: config_bool("notify"),
                notification_poster: config_bool("notification_poster"),
                display_time_ms,
                update_library: config_bool("update_library"),
                clean_library: config_bool("clean_library"),
                always_update: config_bool("always_update"),
                show_dialogs: config_bool("show_dialogs"),
            },
            warnings,
        ))
    }
}

/// The one thing this channel has to get right before anything else: an origin
/// Scryer will actually let it reach.
///
/// `allowed_hosts_for_descriptor` (`crates/scryer-plugins/src/loader.rs:3143`)
/// unions the descriptor's static hosts with the hostname of every configuration
/// value that `url::Url::parse` accepts *and* that has a host. This channel
/// declares no static host, so `server_url` is the only thing that can put an
/// origin in the allowlist — and an empty allowlist denies every request
/// (`plugin_http_host.rs:772-797`). Sonarr's `Host` is a bare hostname and never
/// parses, which is why it is a fallback here rather than the primary setting.
fn resolve_endpoint(
    server_url: Option<&str>,
    host: Option<&str>,
    port: Option<&str>,
    use_ssl: bool,
    url_base: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<(String, bool), PluginError> {
    let (url_base, url_base_is_default) = normalized_url_base(url_base);

    if let Some(server_url) = server_url {
        if host.is_some() {
            warnings.push(
                "both server_url and the legacy host setting are configured; server_url is used"
                    .to_string(),
            );
        }
        return Ok((
            join_url_base(
                server_url,
                &url_base,
                url_base_is_default,
                "server_url",
                warnings,
            )?,
            false,
        ));
    }

    let Some(host) = host else {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "kodi has no server address: set server_url to Kodi's web server, for example http://kodi.local:8080 (the legacy host and port settings are also accepted)"
                .to_string(),
            None,
        ));
    };

    // An operator who pasted a URL into the legacy field meant a URL. Take it,
    // and say so, rather than building `http://http://kodi.local:8080`.
    if host.contains("://") {
        warnings.push(format!(
            "the legacy host setting holds a URL ({host}); it was used as the server URL, but move it to server_url so Scryer allows requests to it"
        ));
        return Ok((
            join_url_base(host, &url_base, url_base_is_default, "host", warnings)?,
            false,
        ));
    }

    let port = match port {
        Some(port) => port.parse::<i64>().map_err(|error| {
            plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("port must be a whole number; got {port:?}"),
                Some(error.to_string()),
            )
        })?,
        None => DEFAULT_PORT,
    };
    if !(1..=65535).contains(&port) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("port must be between 1 and 65535; got {port}"),
            None,
        ));
    }

    let scheme = if use_ssl { "https" } else { "http" };
    warnings.push(format!(
        "this channel is configured with the legacy host and port settings. Scryer builds a plugin's network allowlist from configuration values that are URLs, so requests may be refused until server_url is set to {scheme}://{host}:{port}"
    ));
    Ok((format!("{scheme}://{host}:{port}{url_base}"), true))
}

/// `RuleFor(c => c.UrlBase).ValidUrlBase()` (`XbmcSettings.cs:16`): a leading
/// slash, no trailing one, and the documented default when unset.
fn normalized_url_base(raw: Option<&str>) -> (String, bool) {
    let trimmed = raw.map(str::trim).unwrap_or("");
    if trimmed.is_empty() || trimmed == "/" {
        return (DEFAULT_URL_BASE.to_string(), true);
    }
    let trimmed = trimmed.trim_end_matches('/');
    let normalized = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    let is_default = normalized == DEFAULT_URL_BASE;
    (normalized, is_default)
}

/// `server_url` may already carry the endpoint path (`http://kodi:8080/jsonrpc`
/// is what an operator copies out of a browser). When it does, it wins and
/// `url_base` is only mentioned if the operator also customised it.
fn join_url_base(
    raw: &str,
    url_base: &str,
    url_base_is_default: bool,
    key: &str,
    warnings: &mut Vec<String>,
) -> Result<String, PluginError> {
    let normalized = normalized_url(raw, key)?;
    let path_at = normalized
        .to_ascii_lowercase()
        .find("//")
        .map(|at| at + 2)
        .unwrap_or(0);
    let has_path = normalized[path_at..].contains('/');
    if has_path {
        if !url_base_is_default {
            warnings.push(format!(
                "{key} already carries a path, so the url_base setting ({url_base}) was not applied"
            ));
        }
        return Ok(normalized);
    }
    Ok(format!("{normalized}{url_base}"))
}

/// An absolute `http(s)` origin with no trailing slash.
fn normalized_url(raw: &str, key: &str) -> Result<String, PluginError> {
    let trimmed = raw.trim().trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "{key} must be an absolute http:// or https:// URL, for example http://kodi.local:8080"
            ),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    let authority_at = lower.find("//").map(|at| at + 2).unwrap_or(0);
    if trimmed.len() <= authority_at {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("{key} has no host, for example http://kodi.local:8080"),
            Some(format!("configured value: {trimmed}")),
        ));
    }
    Ok(trimmed.to_string())
}

/// Sonarr sends the credential only when the username is set
/// (`XbmcJsonApiProxy.cs:85-88`); a password without a username is silently
/// dropped there. Kodi's web server always pairs the two, so the same rule
/// applies here — but a password-only configuration is a mistake worth naming,
/// which the classifier does when the 401 arrives.
fn resolve_auth(username: Option<&str>, password: Option<&str>) -> Option<String> {
    let username = username?;
    Some(basic_auth_header(username, password.unwrap_or_default()))
}

/// Sonarr refuses a display time below 2 seconds when the form is saved and
/// never checks it again. Kodi's own floor is 1500 ms, below which
/// `GUI.ShowNotification` answers `Invalid params` and the notification is lost.
fn resolve_display_time_ms(
    raw: Option<&str>,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<i64, PluginError> {
    let seconds = match raw {
        None => DEFAULT_DISPLAY_TIME_SECONDS,
        Some(raw) => raw.parse::<i64>().map_err(|error| {
            plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("display_time must be a whole number of seconds; got {raw:?}"),
                Some(error.to_string()),
            )
        })?,
    };

    if seconds >= MIN_DISPLAY_TIME_SECONDS {
        return Ok(seconds * 1000);
    }

    let message = format!(
        "display_time must be at least {MIN_DISPLAY_TIME_SECONDS} seconds; got {seconds}. Kodi rejects a display time below {KODI_MIN_DISPLAY_TIME_MS} ms outright."
    );
    if strict {
        return Err(plugin_error(PluginErrorCode::InvalidConfig, message, None));
    }
    warnings.push(format!(
        "{message} It was raised to {MIN_DISPLAY_TIME_SECONDS} seconds for this notification."
    ));
    Ok(MIN_DISPLAY_TIME_SECONDS * 1000)
}

// ---------------------------------------------------------------------------
// Event → library action
// ---------------------------------------------------------------------------

/// What one event asks of Kodi, before the channel's own switches are applied.
///
/// Sonarr's table lives in `Xbmc.cs:28-100`: grab notifies; download notifies
/// and updates, cleaning only when files were replaced (`OnDownload`,
/// `message.OldFiles.Any()`); import-complete notifies, updates and cleans;
/// rename updates and cleans without notifying; an episode-file delete and a
/// series add both notify, update and clean; a series delete does all three but
/// **only** when it deleted files; health, application-update and
/// manual-interaction notify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LibraryPlan {
    notify: bool,
    scan: bool,
    clean: bool,
}

/// Sonarr's `OnDownload` is Scryer's `ImportComplete`/`Upgrade`, **not** its
/// `Download`: the dispatcher maps `DownloadFailed` onto
/// `NotificationEventType::Download` (`dispatcher.rs:418-447`), so that arm is a
/// failure notification and must never touch the library.
fn library_plan(req: &Request) -> LibraryPlan {
    let notify_only = LibraryPlan {
        notify: true,
        ..LibraryPlan::default()
    };
    match req.event_type {
        // Nothing has changed on disk yet.
        NotificationEventType::Grab => notify_only,
        // A *failed* download. Sonarr's OnDownload equivalent is below.
        NotificationEventType::Download => notify_only,
        NotificationEventType::ImportComplete => LibraryPlan {
            notify: true,
            scan: true,
            clean: replaced_existing_files(req),
        },
        // An upgrade replaced a file by definition, which is Sonarr's
        // `OldFiles.Any()` case.
        NotificationEventType::Upgrade => LibraryPlan {
            notify: true,
            scan: true,
            clean: true,
        },
        // `OnRename` updates and cleans without notifying (`Xbmc.cs:50-53`).
        NotificationEventType::Rename => LibraryPlan {
            notify: false,
            scan: true,
            clean: true,
        },
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            LibraryPlan {
                notify: true,
                scan: true,
                clean: true,
            }
        }
        NotificationEventType::TitleAdded => LibraryPlan {
            notify: true,
            scan: true,
            clean: true,
        },
        // Sonarr acts on a series delete only when it deleted files
        // (`Xbmc.cs:70-79`). The contract carries no "files deleted" flag, so
        // the deleted paths stand in for it: with them, this is Sonarr's case;
        // without them the entry simply left Scryer and a scan is enough.
        NotificationEventType::TitleDeleted => LibraryPlan {
            notify: true,
            scan: true,
            clean: !deleted_paths(req).is_empty(),
        },
        // Scryer-only events with no library consequence, plus Sonarr's
        // notify-only set.
        NotificationEventType::ImportRejected
        | NotificationEventType::PostProcessingCompleted
        | NotificationEventType::SubtitleDownloaded
        | NotificationEventType::SubtitleSearchFailed
        | NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestApproved
        | NotificationEventType::MediaRequestRejected
        | NotificationEventType::MediaRequestCanceled
        | NotificationEventType::HealthIssue
        | NotificationEventType::HealthRestored
        | NotificationEventType::ApplicationUpdate
        | NotificationEventType::ManualInteractionRequired
        | NotificationEventType::Test => notify_only,
    }
}

/// The event's plan narrowed by this channel's own three switches.
///
/// Sonarr applies them in `Xbmc.Notify` (`if (Settings.Notify)`), in
/// `ProcessQueue` (`if (Settings.UpdateLibrary)` / `Settings.CleanLibrary`) and
/// in `UpdateAndClean`, which does not even queue the item unless one of the two
/// library switches is on. The one exception is a connection test, which
/// notifies regardless (`XbmcService.cs:127-140`) so the operator can *see* that
/// the channel works — the June port only notified when `notify` was on, so a
/// library-only channel tested green without touching Kodi at all.
fn effective_plan(req: &Request, settings: &Settings) -> LibraryPlan {
    let plan = library_plan(req);
    LibraryPlan {
        notify: (plan.notify && settings.notify) || req.is_test,
        scan: plan.scan && settings.update_library,
        clean: plan.clean && settings.clean_library,
    }
}

/// Did this import replace something that is now stale in Kodi's database?
///
/// `import.replaced_paths`/`deleted_paths` are the contract's own answer, and
/// `import.upgrade` says it directly. Neither is populated by the dispatcher
/// today (see the report), so a `Deleted` media update is the shape that
/// actually arrives.
fn replaced_existing_files(req: &Request) -> bool {
    if let Some(import) = &req.import
        && (import.upgrade || !import.replaced_paths.is_empty() || !import.deleted_paths.is_empty())
    {
        return true;
    }
    !deleted_paths(req).is_empty()
}

fn deleted_paths(req: &Request) -> Vec<String> {
    let mut paths: Vec<String> = req
        .file
        .as_ref()
        .map(|file| {
            file.media_updates
                .iter()
                .filter(|update| update.update_type == NotificationMediaUpdateType::Deleted)
                .map(|update| update.path.trim().to_string())
                .filter(|path| !path.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if let Some(import) = &req.import {
        for path in import
            .deleted_paths
            .iter()
            .chain(import.replaced_paths.iter())
        {
            let path = path.trim().to_string();
            if !path.is_empty() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// Sonarr's per-event constants, branded with the application's own name
/// (`Xbmc.cs:28-100`, `NotificationBase.cs:19-33`). Scryer's `summary_title`
/// carries the title name too, but a Kodi toast puts the header on one clipped
/// line above the message that already names the title, so the short constant
/// is the right thing there.
fn notification_header(req: &Request) -> String {
    let app = req.app.name.trim();
    let app = if app.is_empty() { "Scryer" } else { app };
    format!("{app} - {}", header_suffix(req.event_type))
}

fn header_suffix(event_type: NotificationEventType) -> &'static str {
    match event_type {
        NotificationEventType::Grab => "Grabbed",
        // Sonarr's "Downloaded". Scryer's `Download` is a failure.
        NotificationEventType::Download => "Download Failed",
        NotificationEventType::ImportComplete => "Imported",
        NotificationEventType::Upgrade => "Upgraded",
        NotificationEventType::ImportRejected => "Import Rejected",
        NotificationEventType::Rename => "Renamed",
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            "Deleted"
        }
        NotificationEventType::TitleAdded => "Added",
        NotificationEventType::TitleDeleted => "Deleted",
        NotificationEventType::PostProcessingCompleted => "Post-processing",
        NotificationEventType::SubtitleDownloaded => "Subtitles Downloaded",
        NotificationEventType::SubtitleSearchFailed => "Subtitle Search Failed",
        NotificationEventType::MediaRequestSubmitted => "Request Submitted",
        NotificationEventType::MediaRequestApproved => "Request Approved",
        NotificationEventType::MediaRequestRejected => "Request Rejected",
        NotificationEventType::MediaRequestCanceled => "Request Canceled",
        NotificationEventType::HealthIssue => "Health Check Failure",
        NotificationEventType::HealthRestored => "Health Check Restored",
        NotificationEventType::ApplicationUpdate => "Updated",
        NotificationEventType::ManualInteractionRequired => "Manual Interaction",
        NotificationEventType::Test => "Test Notification",
    }
}

/// The toast body.
///
/// `summary_message` is the dispatcher's own prose and is what Sonarr sends. The
/// one enrichment that fits on a clipped line is the quality, because "Imported
/// 1 file for 'X'." does not say *which* file won.
fn notification_message(req: &Request, warnings: &mut Vec<String>) -> String {
    let summary = collapse_whitespace(&req.summary_message);
    let mut message = if summary.is_empty() {
        collapse_whitespace(&req.summary_title)
    } else {
        summary
    };

    if matches!(
        req.event_type,
        NotificationEventType::Grab
            | NotificationEventType::ImportComplete
            | NotificationEventType::Upgrade
    ) && let Some(quality) = quality(req)
        && !message
            .to_ascii_lowercase()
            .contains(&quality.to_ascii_lowercase())
    {
        message = format!("{message} ({quality})");
    }

    truncate_chars(&message, MAX_MESSAGE_CHARS, warnings)
}

fn quality(req: &Request) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.quality.clone()),
    )
    .or_else(|| {
        req.media_files
            .iter()
            .find_map(|file| non_empty(file.quality.clone()))
    })
}

/// A Kodi toast is one paragraph on top of the video; embedded newlines are
/// rendered as spaces by most skins and as nothing by some.
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(value: &str, budget: usize, warnings: &mut Vec<String>) -> String {
    let length = value.chars().count();
    if length <= budget {
        return value.to_string();
    }
    warnings.push(format!(
        "the notification text was {length} characters and was truncated to {budget} so Kodi's on-screen notification can show it"
    ));
    let mut out: String = value.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `GUI.ShowNotification`'s `image`: either one of Kodi's own badges or a path.
///
/// The June port hard-coded a raw.githubusercontent.com URL that 404s. The
/// dispatcher already stamps a severity (`dispatcher.rs:895`,
/// `notification_severity`), and Kodi's enum is spelled exactly the same way, so
/// the badge is free. A poster is an opt-in because Kodi has to fetch it.
fn notification_image(req: &Request, settings: &Settings) -> String {
    if settings.notification_poster
        && let Some(poster) = poster_url(req)
        && is_absolute_http(&poster)
    {
        return poster;
    }
    req.severity
        .unwrap_or_else(|| default_severity(req.event_type))
        .as_str()
        .to_string()
}

/// The dispatcher fills `severity` on everything it builds
/// (`dispatcher.rs:895`); this is the answer for a request that predates it or
/// arrives from a test harness.
fn default_severity(event_type: NotificationEventType) -> NotificationSeverity {
    match event_type {
        NotificationEventType::Download
        | NotificationEventType::ImportRejected
        | NotificationEventType::SubtitleSearchFailed => NotificationSeverity::Error,
        NotificationEventType::HealthIssue | NotificationEventType::ManualInteractionRequired => {
            NotificationSeverity::Warning
        }
        _ => NotificationSeverity::Info,
    }
}

fn is_absolute_http(link: &str) -> bool {
    let link = link.to_ascii_lowercase();
    link.starts_with("http://") || link.starts_with("https://")
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Library lookup
// ---------------------------------------------------------------------------

/// Which half of Kodi's video library a title lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Facet {
    Movie,
    Series,
    /// `MediaFacet::Anime` is its own facet in Scryer and can be either, so the
    /// lookup tries tv shows and the clean stays unscoped.
    Ambiguous,
}

/// `MediaFacet::as_str` (`scryer-domain/src/lib.rs:45-51`) is the source of
/// `movie`/`series`/`anime`. Anything else — including the `tv` that older
/// fixtures carry — is treated as a series, which is what the June port assumed
/// unconditionally.
fn facet_of(title: Option<&PluginNotificationTitle>) -> Facet {
    match title
        .map(|title| title.facet.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("movie") | Some("movies") | Some("film") => Facet::Movie,
        Some("anime") => Facet::Ambiguous,
        _ => Facet::Series,
    }
}

/// `VideoLibrary.Clean`'s `content` enum for this facet, or `None` when the
/// clean has to stay global.
fn clean_content(facet: Facet) -> Option<&'static str> {
    match facet {
        Facet::Movie => Some("movies"),
        Facet::Series => Some("tvshows"),
        Facet::Ambiguous => None,
    }
}

/// The `properties` a lookup asks for.
///
/// `uniqueid` is a `Media.UniqueID` string map and is the id Kodi's scrapers
/// actually write; `imdbnumber` is the single legacy field Sonarr reads, which
/// for a tv show holds the **TVDB** id when the TVDB scraper populated it. Both
/// are requested when the server is new enough for the former.
fn lookup_properties(version: Option<JsonRpcVersion>, facet: Facet) -> Vec<&'static str> {
    let mut properties = vec!["file", "imdbnumber"];
    if version.is_none_or(JsonRpcVersion::supports_uniqueid) {
        properties.push("uniqueid");
    }
    if facet == Facet::Movie {
        properties.push("year");
    }
    properties
}

/// Sonarr matches a show on `int.TryParse(s.ImdbNumber) == series.TvdbId ||
/// s.Label == series.Title` (`XbmcService.cs:71-76`). That is two of the six
/// identities Scryer carries, and the label comparison alone is what makes
/// "The Office" ambiguous.
fn match_tv_show(shows: &[Value], title: &PluginNotificationTitle) -> Option<String> {
    let ids = &title.external_ids;
    let matchers: Vec<Matcher> = vec![
        id_matcher(unique_id_getter("tvdb"), ids.tvdb_id.clone(), numeric_eq),
        // Sonarr's rule, kept as a fallback: Kodi's TVDB scraper writes the
        // TVDB id into `imdbnumber` for tv shows.
        id_matcher(
            string_getter(&["imdbnumber", "ImdbNumber"]),
            ids.tvdb_id.clone(),
            numeric_eq,
        ),
        id_matcher(unique_id_getter("imdb"), ids.imdb_id.clone(), imdb_eq),
        id_matcher(
            string_getter(&["imdbnumber", "ImdbNumber"]),
            ids.imdb_id.clone(),
            imdb_eq,
        ),
        id_matcher(unique_id_getter("tmdb"), ids.tmdb_id.clone(), numeric_eq),
        label_matcher(title.name.clone(), None),
    ];

    first_match(shows, &matchers).and_then(|show| string_member(show, &["file", "File"]))
}

/// Sonarr has no movie support at all; this is `GetSeriesPath`'s shape applied
/// to `VideoLibrary.GetMovies`, whose `file` is the movie file rather than a
/// folder.
fn match_movie(movies: &[Value], title: &PluginNotificationTitle) -> Option<String> {
    let ids = &title.external_ids;
    let matchers: Vec<Matcher> = vec![
        id_matcher(unique_id_getter("tmdb"), ids.tmdb_id.clone(), numeric_eq),
        id_matcher(unique_id_getter("imdb"), ids.imdb_id.clone(), imdb_eq),
        id_matcher(
            string_getter(&["imdbnumber", "ImdbNumber"]),
            ids.imdb_id.clone(),
            imdb_eq,
        ),
        id_matcher(unique_id_getter("tvdb"), ids.tvdb_id.clone(), numeric_eq),
        label_matcher(title.name.clone(), title.year),
    ];

    first_match(movies, &matchers).and_then(|movie| string_member(movie, &["file", "File"]))
}

fn first_match<'a>(entries: &'a [Value], matchers: &[Matcher]) -> Option<&'a Value> {
    matchers
        .iter()
        .find_map(|matcher| entries.iter().find(|entry| matcher(entry)))
}

fn id_matcher(
    getter: Getter,
    expected: Option<String>,
    compare: fn(&str, &str) -> bool,
) -> Matcher {
    Box::new(move |entry| {
        let Some(expected) = expected.as_deref() else {
            return false;
        };
        getter(entry).is_some_and(|actual| compare(&actual, expected))
    })
}

/// The last resort, and the one that can be wrong: two titles with the same
/// name. A year narrows it when both sides have one, which is why movies pass
/// theirs.
fn label_matcher(name: String, year: Option<i32>) -> Matcher {
    Box::new(move |entry| {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let label_matches = string_member(entry, &["label", "Label", "title", "Title"])
            .is_some_and(|label| label.eq_ignore_ascii_case(name));
        if !label_matches {
            return false;
        }
        match (year, entry.get("year").and_then(Value::as_i64)) {
            (Some(expected), Some(actual)) => i64::from(expected) == actual,
            _ => true,
        }
    })
}

fn unique_id_getter(source: &'static str) -> Getter {
    Box::new(move |entry| {
        let unique = entry.get("uniqueid").or_else(|| entry.get("UniqueId"))?;
        string_member(unique, &[source])
    })
}

fn string_getter(keys: &'static [&'static str]) -> Getter {
    Box::new(move |entry| string_member(entry, keys))
}

fn numeric_eq(actual: &str, expected: &str) -> bool {
    match (actual.trim().parse::<i64>(), expected.trim().parse::<i64>()) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => false,
    }
}

/// Kodi stores IMDb ids with the `tt` prefix; Scryer's may or may not carry it.
fn imdb_eq(actual: &str, expected: &str) -> bool {
    let normalize = |value: &str| {
        value
            .trim()
            .to_ascii_lowercase()
            .trim_start_matches("tt")
            .to_string()
    };
    let actual = normalize(actual);
    !actual.is_empty() && actual == normalize(expected)
}

fn string_member(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| {
            value.get(*key).and_then(|value| match value {
                Value::String(value) => Some(value.trim().to_string()),
                Value::Number(value) => Some(value.to_string()),
                Value::Bool(value) => Some(value.to_string()),
                _ => None,
            })
        })
        .filter(|value| !value.is_empty())
}

/// Kodi's virtual file systems are not directories.
///
/// `stack://` concatenates several files with " , " separators, `multipath://`
/// URL-encodes a list, and the archive schemes address a file *inside* a
/// container. Handing any of them to `VideoLibrary.Scan` scans nothing, so the
/// lookup gives up and the scan falls back to the whole library — which is what
/// Sonarr does whenever it cannot find the show.
const VIRTUAL_PATH_SCHEMES: [&str; 5] =
    ["stack://", "multipath://", "rar://", "zip://", "videodb://"];

/// The directory a movie file lives in. A tv show's `file` is already a folder.
fn parent_directory(path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    if VIRTUAL_PATH_SCHEMES
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return None;
    }
    // Already a directory — Kodi ends folder paths with the separator.
    if path.ends_with('/') || path.ends_with('\\') {
        return Some(path.to_string());
    }
    let cut = path.rfind(['/', '\\'])?;
    // `smb://host` has no directory above it, and neither does `/file`.
    let directory = &path[..=cut];
    let authority_end = lower.find("//").map(|at| at + 2);
    if authority_end.is_some_and(|end| cut < end) {
        return None;
    }
    Some(directory.to_string())
}

// ---------------------------------------------------------------------------
// JSON-RPC parameters
// ---------------------------------------------------------------------------

fn show_notification_params(req: &Request, settings: &Settings, message: &str) -> Value {
    json!({
        "title": notification_header(req),
        "message": message,
        "image": notification_image(req, settings),
        "displaytime": settings.display_time_ms.max(KODI_MIN_DISPLAY_TIME_MS),
    })
}

/// `VideoLibrary.Scan(directory, showdialogs)`.
///
/// Sonarr sends the directory positionally and omits `showdialogs`, so Kodi's
/// default puts a progress dialog on screen for every automated scan
/// (`XbmcJsonApiProxy.cs:41-55`). Named parameters make the omission of the
/// directory explicit and let this channel suppress the dialog.
fn scan_params(directory: Option<&str>, settings: &Settings) -> Value {
    let mut params = Map::new();
    if let Some(directory) = directory {
        params.insert(
            "directory".to_string(),
            Value::String(directory.to_string()),
        );
    }
    params.insert(
        "showdialogs".to_string(),
        Value::Bool(settings.show_dialogs),
    );
    Value::Object(params)
}

/// `VideoLibrary.Clean(showdialogs, content, directory)`.
///
/// Sonarr sends no parameters at all, so every clean is a full pass over the
/// whole video database (`XbmcJsonApiProxy.cs:57-60`). `content` and
/// `directory` narrow that to the half of the library — and, when Kodi is new
/// enough and the folder was found, the one folder — the event actually
/// touched.
fn clean_params(
    facet: Facet,
    directory: Option<&str>,
    version: Option<JsonRpcVersion>,
    settings: &Settings,
) -> Value {
    let mut params = Map::new();
    params.insert(
        "showdialogs".to_string(),
        Value::Bool(settings.show_dialogs),
    );
    if version.is_some_and(JsonRpcVersion::supports_clean_content)
        && let Some(content) = clean_content(facet)
    {
        params.insert("content".to_string(), Value::String(content.to_string()));
    }
    if version.is_some_and(JsonRpcVersion::supports_clean_directory)
        && let Some(directory) = directory
    {
        params.insert(
            "directory".to_string(),
            Value::String(directory.to_string()),
        );
    }
    Value::Object(params)
}

/// `Player.Type` is `video`/`audio`/`picture`; Sonarr compares against "video"
/// (`XbmcService.cs:124`).
fn has_active_video_player(result: &Value) -> bool {
    result.as_array().into_iter().flatten().any(|player| {
        player
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case("video"))
    })
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// What one JSON-RPC call produced.
enum CallOutcome {
    /// The `result` member, which is `"OK"` for the mutating methods and an
    /// object or array for the readers.
    Ok(Value),
    /// The channel itself is misconfigured. Carries the typed error so a send in
    /// which *every* call says the same thing can be reported on the typed lane
    /// naming the field.
    Misconfigured(PluginError),
    /// Kodi said no, for now.
    Rejected {
        detail: String,
        provider_status: String,
        retry_after_seconds: Option<i64>,
    },
}

impl CallOutcome {
    fn result(&self) -> Option<&Value> {
        match self {
            Self::Ok(result) => Some(result),
            _ => None,
        }
    }

    fn failure(&self) -> Option<String> {
        match self {
            Self::Ok(_) => None,
            Self::Misconfigured(error) => Some(error.public_message.clone()),
            Self::Rejected { detail, .. } => Some(detail.clone()),
        }
    }

    fn provider_status(&self) -> Option<String> {
        match self {
            Self::Ok(_) => Some("ok".to_string()),
            Self::Misconfigured(error) => Some(format!("{:?}", error.code).to_ascii_lowercase()),
            Self::Rejected {
                provider_status, ..
            } => Some(provider_status.clone()),
        }
    }
}

fn json_rpc(settings: &Settings, method: &str, params: Value, strict: bool) -> CallOutcome {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body = match serde_json::to_vec(&body) {
        Ok(body) => body,
        Err(error) => {
            return CallOutcome::Misconfigured(plugin_error(
                PluginErrorCode::Permanent,
                format!("could not encode the {method} request"),
                Some(error.to_string()),
            ));
        }
    };

    let mut request = HttpRequest::new(&settings.endpoint)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    if let Some(auth) = &settings.auth {
        request = request.with_header("Authorization", auth);
    }

    match http::request::<Vec<u8>>(&request, Some(body)) {
        Ok(response) => classify_response(
            response.status_code(),
            retry_after(response.headers()),
            &response.body(),
            method,
            settings,
        ),
        Err(error) => transport_failure(&error.to_string(), method, settings, strict),
    }
}

/// Sonarr catches a `SocketException` per call and logs it at Debug
/// (`Xbmc.cs:143-149`), so an unreachable Kodi is invisible. Scryer's host
/// answers a refused or failed egress in-band, so there are two cases: the host
/// would not let this plugin out at all — a configuration problem with a precise
/// fix, typed on every send — and Kodi not answering. The latter is typed
/// `UpstreamUnavailable` on a connection test, where Sonarr would blame `Host`
/// (`XbmcService.cs:136`), and stays on the delivery lane on a live send: a
/// network blink must not be reported to the operator as a broken setting.
fn transport_failure(error: &str, method: &str, settings: &Settings, strict: bool) -> CallOutcome {
    let lower = error.to_ascii_lowercase();
    if lower.contains("is not allowed") || lower.contains("not permitted") {
        return CallOutcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "Scryer refused a request to {}: set server_url to that address so it is added to this channel's allowlist. Scryer derives the allowlist from configuration values that are URLs, and the legacy host setting is not one.",
                settings.endpoint
            ),
            Some(format!("{method}: {error}")),
        ));
    }

    let detail = format!(
        "could not reach Kodi at {}: {}",
        settings.endpoint,
        ellipsize(error, MAX_QUOTED_ERROR)
    );
    if strict {
        return CallOutcome::Misconfigured(plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            format!(
                "{detail}. Check server_url{}, and that Kodi has 'Allow remote control via HTTP' enabled.",
                if settings.legacy_connection {
                    " (or the legacy host, port and use_ssl settings)"
                } else {
                    ""
                }
            ),
            Some(format!("{method}: {error}")),
        ));
    }
    CallOutcome::Rejected {
        detail,
        provider_status: "request_failed".to_string(),
        retry_after_seconds: None,
    }
}

/// Sonarr folds all of this into one `XbmcJsonException` message
/// (`XbmcJsonApiProxy.cs:98-116`), and its `ErrorResult.Error` is a
/// `Dictionary<string, string>` that cannot even deserialise a JSON-RPC error
/// carrying a structured `data` member.
fn classify_response(
    status: u16,
    retry_after_seconds: Option<i64>,
    body: &[u8],
    method: &str,
    settings: &Settings,
) -> CallOutcome {
    let raw = String::from_utf8_lossy(body).to_string();
    let parsed: Option<Value> = serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object);
    let quoted = quoted_body(&raw, status);
    let debug = format!("{method} → HTTP {status}: {quoted}");

    // Kodi's web server answers these as HTML, so they are decided before the
    // "did not answer JSON" branch that would otherwise swallow them.
    match status {
        401 | 403 => {
            return CallOutcome::Misconfigured(plugin_error(
                PluginErrorCode::AuthFailed,
                format!(
                    "Kodi rejected the credentials (HTTP {status}): check username and password. These are Kodi's web-server credentials from Settings → Services → Control, not a Kodi profile."
                ),
                Some(debug),
            ));
        }
        404 => {
            return CallOutcome::Misconfigured(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "{} does not exist (HTTP 404): Kodi serves JSON-RPC at {DEFAULT_URL_BASE}. Check url_base, and any path in server_url.",
                    settings.endpoint
                ),
                Some(debug),
            ));
        }
        429 | 500..=599 => {
            return CallOutcome::Rejected {
                detail: format!("Kodi answered HTTP {status} for {method}: {quoted}"),
                provider_status: format!("http_{status}"),
                retry_after_seconds,
            };
        }
        _ => {}
    }

    // Kodi answers JSON on every documented status. Anything else on this
    // endpoint is the web *interface* — Kodi's own Chorus skin, or a reverse
    // proxy's login page — which is a `url_base` problem with a precise fix.
    let Some(parsed) = parsed else {
        return CallOutcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "{} did not answer with JSON-RPC (HTTP {status}): {quoted}. That is usually Kodi's web interface rather than its JSON-RPC endpoint — check url_base, and any path in server_url.",
                settings.endpoint
            ),
            Some(debug),
        ));
    };

    // A JSON-RPC error arrives inside a 200. Sonarr's wording, with the method
    // added because this channel makes up to four calls per event.
    if let Some(error) = parsed.get("error") {
        let code = error
            .get("code")
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message");
        return CallOutcome::Misconfigured(plugin_error(
            PluginErrorCode::Permanent,
            format!("Kodi JSON error. Code = {code}, Message: {message} (method {method})"),
            Some(format!("{debug} :: {error}")),
        ));
    }

    if !(200..300).contains(&status) {
        return CallOutcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("Kodi rejected {method} with HTTP {status}: {quoted}"),
            Some(debug),
        ));
    }

    CallOutcome::Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
}

fn quoted_body(raw: &str, status: u16) -> String {
    match raw.trim() {
        "" => format!("HTTP {status} with an empty body"),
        body => ellipsize(&collapse_whitespace(body), MAX_QUOTED_ERROR),
    }
}

fn ellipsize(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn retry_after(headers: &std::collections::BTreeMap<String, String>) -> Option<i64> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<i64>().ok())
        .filter(|seconds| *seconds >= 0)
        .map(|seconds| seconds.max(1))
}

fn plugin_error(
    code: PluginErrorCode,
    public_message: String,
    debug_message: Option<String>,
) -> PluginError {
    PluginError {
        code,
        public_message,
        debug_message,
        retry_after_seconds: None,
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// One JSON-RPC call and what it produced, reported per method so an operator
/// can see that the toast worked and the clean did not.
struct Call {
    method: &'static str,
    outcome: CallOutcome,
}

fn send_notification(req: &Request) -> PluginResult<PluginNotificationResponse> {
    let (settings, mut warnings) = match Settings::from_config(req.is_test) {
        Ok(resolved) => resolved,
        Err(error) => return PluginResult::Err(error),
    };

    let plan = effective_plan(req, &settings);
    let mut calls: Vec<Call> = Vec::new();
    let mut version: Option<JsonRpcVersion> = None;

    // Sonarr's `Test` always notifies (`XbmcService.cs:127-140`), whatever the
    // GUI-notification setting says. `JSONRPC.Version` runs first because it is
    // the cheapest proof that the address and the credentials are right, and
    // because it decides which `VideoLibrary.Clean` parameters exist.
    if req.is_test {
        let outcome = json_rpc(&settings, "JSONRPC.Version", json!({}), true);
        if let Some(result) = outcome.result() {
            match parse_jsonrpc_version(result) {
                Some(parsed) => {
                    version = Some(parsed);
                    warnings.extend(version_warnings(parsed));
                }
                None => warnings.push(
                    "JSONRPC.Version answered without a version; this may not be a Kodi host"
                        .to_string(),
                ),
            }
        }
        calls.push(Call {
            method: "JSONRPC.Version",
            outcome,
        });
    }

    if plan.notify {
        let message = notification_message(req, &mut warnings);
        let params = show_notification_params(req, &settings, &message);
        calls.push(Call {
            method: "GUI.ShowNotification",
            outcome: json_rpc(&settings, "GUI.ShowNotification", params, req.is_test),
        });
    }

    if plan.scan || plan.clean {
        run_library_actions(
            req,
            &settings,
            plan.scan,
            plan.clean,
            &mut version,
            &mut calls,
            &mut warnings,
        );
    }

    if calls.is_empty() {
        let mut response = ok_response();
        response.warnings = vec![format!(
            "nothing to do for a {} event: GUI Notification, Update Library and Clean Library are all off for this channel",
            req.event_type.as_str()
        )];
        return PluginResult::Ok(response);
    }

    finish(calls, warnings)
}

/// Sonarr checks for an active player once per queued item and skips *both* the
/// update and the clean when one is playing (`XbmcService.cs:37-59`,
/// `CheckIfVideoPlayerOpen`). The June port asked Kodi twice, once per action,
/// and skipped silently — the operator saw a successful notification and no
/// scan, with nothing to explain it.
fn run_library_actions(
    req: &Request,
    settings: &Settings,
    scan: bool,
    clean: bool,
    version: &mut Option<JsonRpcVersion>,
    calls: &mut Vec<Call>,
    warnings: &mut Vec<String>,
) {
    if !settings.always_update {
        let outcome = json_rpc(settings, "Player.GetActivePlayers", json!({}), false);
        match outcome.result() {
            Some(result) => {
                if has_active_video_player(result) {
                    warnings.push(
                        "Kodi is playing video, so the library scan and clean were skipped; enable always_update to run them anyway".to_string(),
                    );
                    return;
                }
            }
            None => {
                warnings.push(format!(
                    "could not tell whether Kodi is playing video, so the library scan and clean were skipped: {}",
                    outcome.failure().unwrap_or_default()
                ));
                calls.push(Call {
                    method: "Player.GetActivePlayers",
                    outcome,
                });
                return;
            }
        }
    }

    let facet = facet_of(req.title.as_ref());
    let directory = resolve_library_directory(req, settings, facet, version, warnings);

    if scan {
        if directory.is_none() {
            warnings.push(
                "the title's folder could not be found in Kodi, so the whole video library is being scanned".to_string(),
            );
        }
        calls.push(Call {
            method: "VideoLibrary.Scan",
            outcome: json_rpc(
                settings,
                "VideoLibrary.Scan",
                scan_params(directory.as_deref(), settings),
                req.is_test,
            ),
        });
    }

    if clean {
        // `content` and `directory` are version-gated, so this is the one place
        // a live send is worth a version probe.
        if version.is_none() {
            *version = probe_version(settings, warnings);
        }
        calls.push(Call {
            method: "VideoLibrary.Clean",
            outcome: json_rpc(
                settings,
                "VideoLibrary.Clean",
                clean_params(facet, directory.as_deref(), *version, settings),
                req.is_test,
            ),
        });
    }
}

/// Best-effort on a live send: an unknown version only costs the two optional
/// `VideoLibrary.Clean` parameters, which is exactly the call Sonarr makes.
fn probe_version(settings: &Settings, warnings: &mut Vec<String>) -> Option<JsonRpcVersion> {
    let outcome = json_rpc(settings, "JSONRPC.Version", json!({}), false);
    match outcome.result().and_then(parse_jsonrpc_version) {
        Some(version) => Some(version),
        None => {
            warnings.push(
                "Kodi did not report its JSON-RPC version, so the library clean was not limited to this title's content type or folder".to_string(),
            );
            None
        }
    }
}

/// `GetSeriesPath` (`XbmcService.cs:61-84`), widened to movies.
///
/// A failure here is advisory: Sonarr swallows it and scans the whole library
/// (`XbmcService.cs:86-112`), and so does this — the notification and the scan
/// still happen, and the operator gets a warning rather than a failed delivery.
fn resolve_library_directory(
    req: &Request,
    settings: &Settings,
    facet: Facet,
    version: &mut Option<JsonRpcVersion>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let title = req.title.as_ref()?;

    let (method, collection) = match facet {
        Facet::Movie => ("VideoLibrary.GetMovies", "movies"),
        // Anime is looked up as a series: that is where Scryer puts it, and a
        // miss only costs a full-library scan.
        Facet::Series | Facet::Ambiguous => ("VideoLibrary.GetTVShows", "tvshows"),
    };

    let params = json!({ "properties": lookup_properties(*version, facet) });
    let outcome = json_rpc(settings, method, params, false);
    let Some(result) = outcome.result() else {
        warnings.push(format!(
            "could not look up the title in Kodi's library: {}",
            outcome.failure().unwrap_or_default()
        ));
        return None;
    };

    let entries = result
        .get(collection)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        // `_logger.Debug("No TV shows returned from Kodi")` (`XbmcService.cs:67`).
        warnings.push(format!("Kodi's video library returned no {collection}"));
        return None;
    }

    let file = match facet {
        Facet::Movie => match_movie(&entries, title),
        Facet::Series | Facet::Ambiguous => match_tv_show(&entries, title),
    }?;

    match facet {
        // `Video.Details.TVShow.file` is the show folder.
        Facet::Series | Facet::Ambiguous => Some(file),
        // `Video.Details.Movie.file` is the movie file.
        Facet::Movie => {
            let directory = parent_directory(&file);
            if directory.is_none() {
                warnings.push(format!(
                    "Kodi stores this movie at {file}, which is not a plain directory, so the whole video library is being scanned"
                ));
            }
            directory
        }
    }
}

/// Every call refused the channel's own configuration, and for the same reason:
/// that is a setting the operator must fix, not a delivery that failed, so it
/// goes on the typed lane naming the field. A partial failure — a toast that
/// worked and a clean that did not — stays on the delivery lane so the part that
/// worked is still reported.
fn finish(calls: Vec<Call>, warnings: Vec<String>) -> PluginResult<PluginNotificationResponse> {
    let misconfigurations: Vec<&PluginError> = calls
        .iter()
        .filter_map(|call| match &call.outcome {
            CallOutcome::Misconfigured(error) => Some(error),
            _ => None,
        })
        .collect();

    if misconfigurations.len() == calls.len()
        && let Some(first) = misconfigurations.first()
        && misconfigurations
            .iter()
            .all(|error| error.code == first.code && error.public_message == first.public_message)
    {
        return PluginResult::Err((*first).clone());
    }

    let mut retry_after_seconds: Option<i64> = None;
    let mut failures: Vec<String> = Vec::new();
    let mut target_results: Vec<PluginNotificationTargetResult> = Vec::new();

    for call in &calls {
        if let CallOutcome::Rejected {
            retry_after_seconds: retry_after,
            ..
        } = &call.outcome
            && let Some(retry_after) = retry_after
        {
            retry_after_seconds =
                Some(retry_after_seconds.map_or(*retry_after, |seen: i64| seen.max(*retry_after)));
        }
        let failure = call.outcome.failure();
        if let Some(failure) = &failure {
            failures.push(format!("{}: {failure}", call.method));
        }
        target_results.push(PluginNotificationTargetResult {
            target: call.method.to_string(),
            success: failure.is_none(),
            status: call.outcome.provider_status(),
            error: failure,
        });
    }

    let mut response = if failures.is_empty() {
        ok_response()
    } else {
        error_response(
            failures.join("; "),
            Some(format!(
                "{}/{} Kodi calls failed",
                failures.len(),
                calls.len()
            )),
        )
    };
    response.retry_after_seconds = retry_after_seconds;
    response.target_results = target_results;
    response.warnings = warnings;
    PluginResult::Ok(response)
}

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// `action` is not one of them: the descriptor advertises no action, so the host
/// does not route one here and the arm answers **in-band** with `Unsupported`
/// rather than trapping. A trap under a component costs the whole instance and
/// replaces the plugin's own diagnosis with a generic ABI failure.
fn handle_notification_command(
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    match command {
        PluginNotificationCommand::Send(request) => {
            PluginNotificationCommandResult::Send(send_notification(&request))
        }
        PluginNotificationCommand::Action(_) => {
            PluginNotificationCommandResult::Action(unsupported_action(PROVIDER_TYPE))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{
        PluginNotificationApp, PluginNotificationExternalIds, PluginNotificationFile,
        PluginNotificationImport, PluginNotificationMediaUpdate, PluginNotificationRelease,
    };

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    fn settings() -> Settings {
        Settings {
            endpoint: "http://kodi.local:8080/jsonrpc".to_string(),
            legacy_connection: false,
            auth: None,
            notify: true,
            notification_poster: false,
            display_time_ms: 5000,
            update_library: true,
            clean_library: true,
            always_update: false,
            show_dialogs: false,
        }
    }

    fn request(event_type: NotificationEventType) -> Request {
        Request {
            schema_version: 1,
            event_type,
            event_id: Some("evt-1".to_string()),
            occurred_at: Some("2026-09-02T12:00:00Z".to_string()),
            correlation_id: None,
            actor: None,
            severity: None,
            is_test: matches!(event_type, NotificationEventType::Test),
            summary_title: "Imported: Example Show".to_string(),
            summary_message: "Imported 1 file for 'Example Show'.".to_string(),
            app: PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "test".to_string(),
            },
            title: Some(title("Example Show", "series")),
            episode: None,
            episodes: Vec::new(),
            file: None,
            media_files: Vec::new(),
            release: None,
            download: None,
            import: None,
            health: None,
            application_update: None,
            manual_interaction: None,
            media_request: None,
        }
    }

    fn title(name: &str, facet: &str) -> PluginNotificationTitle {
        PluginNotificationTitle {
            id: None,
            name: name.to_string(),
            facet: facet.to_string(),
            year: None,
            slug: None,
            path: None,
            overview: None,
            sort_title: None,
            background_url: None,
            poster_url: None,
            tags: Vec::new(),
            aliases: Vec::new(),
            original_language: None,
            original_country: None,
            external_ids: PluginNotificationExternalIds::default(),
        }
    }

    fn deleted_update(path: &str) -> PluginNotificationFile {
        PluginNotificationFile {
            primary_path: Some(path.to_string()),
            media_updates: vec![PluginNotificationMediaUpdate {
                path: path.to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        }
    }

    fn version(major: i64, minor: i64) -> JsonRpcVersion {
        JsonRpcVersion {
            major,
            minor,
            patch: 0,
        }
    }

    // -----------------------------------------------------------------------
    // Descriptor and the rename
    // -----------------------------------------------------------------------

    #[test]
    fn descriptor_is_kodi_and_still_answers_to_the_xbmc_id() {
        let descriptor = build_descriptor();
        assert_eq!(descriptor.id, "kodi");
        assert_eq!(descriptor.name, "Kodi");
        let ProviderDescriptor::Notification(notification) = descriptor.provider else {
            panic!("expected a notification descriptor");
        };
        assert_eq!(notification.provider_type, "kodi");
        assert_eq!(notification.provider_aliases, vec!["xbmc".to_string()]);
        // The core coalesces nothing for a plugin channel, so neither may be
        // advertised.
        assert!(!notification.capabilities.supports_batch);
        assert!(!notification.capabilities.supports_coalescing);
        assert!(notification.capabilities.supports_test);
        assert!(!notification.capabilities.requires_host_process);
        assert!(!notification.capabilities.requires_host_filesystem);
    }

    #[test]
    fn config_keys_from_the_xbmc_release_are_all_still_present() {
        let keys: Vec<String> = config_fields().into_iter().map(|field| field.key).collect();
        for key in [
            "host",
            "port",
            "use_ssl",
            "url_base",
            "username",
            "password",
            "display_time",
            "notify",
            "update_library",
            "always_update",
            "clean_library",
        ] {
            assert!(keys.contains(&key.to_string()), "{key} was dropped");
        }
        assert!(keys.contains(&"server_url".to_string()));
        assert!(keys.contains(&"show_dialogs".to_string()));
        assert!(keys.contains(&"notification_poster".to_string()));
    }

    #[test]
    fn server_url_is_the_connection_field_that_builds_the_allowlist() {
        let field = config_fields()
            .into_iter()
            .find(|field| field.key == "server_url")
            .expect("server_url");
        assert_eq!(field.role, Some(ConfigFieldRole::ConnectionUrl));
    }

    // -----------------------------------------------------------------------
    // Endpoint resolution
    // -----------------------------------------------------------------------

    #[test]
    fn server_url_without_a_path_gets_the_default_url_base() {
        let mut warnings = Vec::new();
        let (endpoint, legacy) = resolve_endpoint(
            Some("http://kodi.local:8080"),
            None,
            None,
            false,
            None,
            &mut warnings,
        )
        .expect("resolved");
        assert_eq!(endpoint, "http://kodi.local:8080/jsonrpc");
        assert!(!legacy);
        assert!(warnings.is_empty());
    }

    #[test]
    fn server_url_carrying_a_path_wins_over_url_base() {
        let mut warnings = Vec::new();
        let (endpoint, _) = resolve_endpoint(
            Some("http://kodi.local:8080/kodi/jsonrpc/"),
            None,
            None,
            false,
            Some("/other"),
            &mut warnings,
        )
        .expect("resolved");
        assert_eq!(endpoint, "http://kodi.local:8080/kodi/jsonrpc");
        assert!(warnings.iter().any(|warning| warning.contains("url_base")));
    }

    #[test]
    fn a_custom_url_base_is_applied_to_a_bare_server_url() {
        let mut warnings = Vec::new();
        let (endpoint, _) = resolve_endpoint(
            Some("https://kodi.example"),
            None,
            None,
            false,
            Some("kodi/jsonrpc"),
            &mut warnings,
        )
        .expect("resolved");
        assert_eq!(endpoint, "https://kodi.example/kodi/jsonrpc");
    }

    #[test]
    fn the_legacy_host_still_resolves_and_names_the_url_to_paste() {
        let mut warnings = Vec::new();
        let (endpoint, legacy) = resolve_endpoint(
            None,
            Some("kodi.local"),
            Some("8081"),
            true,
            None,
            &mut warnings,
        )
        .expect("resolved");
        assert_eq!(endpoint, "https://kodi.local:8081/jsonrpc");
        assert!(legacy);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("https://kodi.local:8081")),
            "the warning must name the URL to paste: {warnings:?}"
        );
    }

    #[test]
    fn a_url_pasted_into_the_legacy_host_is_used_as_one() {
        let mut warnings = Vec::new();
        let (endpoint, legacy) = resolve_endpoint(
            None,
            Some("http://kodi.local:8080"),
            Some("9999"),
            false,
            None,
            &mut warnings,
        )
        .expect("resolved");
        assert_eq!(endpoint, "http://kodi.local:8080/jsonrpc");
        assert!(!legacy);
    }

    #[test]
    fn no_address_at_all_is_invalid_config_naming_server_url() {
        let mut warnings = Vec::new();
        let error =
            resolve_endpoint(None, None, None, false, None, &mut warnings).expect_err("must fail");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));
    }

    #[test]
    fn a_non_numeric_port_is_invalid_config() {
        let mut warnings = Vec::new();
        let error = resolve_endpoint(None, Some("kodi"), Some("http"), false, None, &mut warnings)
            .expect_err("must fail");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn a_server_url_without_a_scheme_is_invalid_config() {
        let mut warnings = Vec::new();
        let error = resolve_endpoint(
            Some("kodi.local:8080"),
            None,
            None,
            false,
            None,
            &mut warnings,
        )
        .expect_err("must fail");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));
    }

    #[test]
    fn both_addresses_configured_prefers_server_url_and_says_so() {
        let mut warnings = Vec::new();
        let (endpoint, _) = resolve_endpoint(
            Some("http://new.local:8080"),
            Some("old.local"),
            None,
            false,
            None,
            &mut warnings,
        )
        .expect("resolved");
        assert_eq!(endpoint, "http://new.local:8080/jsonrpc");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("server_url"))
        );
    }

    // -----------------------------------------------------------------------
    // Display time
    // -----------------------------------------------------------------------

    #[test]
    fn display_time_defaults_to_sonarrs_five_seconds() {
        let mut warnings = Vec::new();
        assert_eq!(
            resolve_display_time_ms(None, false, &mut warnings).expect("resolved"),
            5000
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn display_time_below_sonarrs_floor_is_refused_at_test_time() {
        let mut warnings = Vec::new();
        let error = resolve_display_time_ms(Some("1"), true, &mut warnings).expect_err("must fail");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn display_time_below_sonarrs_floor_is_clamped_on_a_live_send() {
        let mut warnings = Vec::new();
        let resolved = resolve_display_time_ms(Some("1"), false, &mut warnings).expect("resolved");
        assert_eq!(resolved, 2000);
        assert!(resolved >= KODI_MIN_DISPLAY_TIME_MS);
        assert_eq!(warnings.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Event → library action (Sonarr Xbmc.cs:28-100)
    // -----------------------------------------------------------------------

    #[test]
    fn grab_notifies_only() {
        assert_eq!(
            library_plan(&request(NotificationEventType::Grab)),
            LibraryPlan {
                notify: true,
                scan: false,
                clean: false
            }
        );
    }

    #[test]
    fn a_failed_download_notifies_and_never_touches_the_library() {
        // `NotificationEventType::Download` is `DownloadFailed`
        // (dispatcher.rs:418-447), not Sonarr's OnDownload.
        let plan = library_plan(&request(NotificationEventType::Download));
        assert!(plan.notify);
        assert!(!plan.scan);
        assert!(!plan.clean);
    }

    #[test]
    fn an_import_that_replaced_nothing_scans_without_cleaning() {
        // Sonarr: `should_not_clean_if_no_episode_was_replaced`
        // (OnDownloadFixture.cs:60-67).
        let plan = library_plan(&request(NotificationEventType::ImportComplete));
        assert!(plan.notify);
        assert!(plan.scan);
        assert!(!plan.clean);
    }

    #[test]
    fn an_import_that_replaced_a_file_also_cleans() {
        // Sonarr: `should_clean_if_episode_was_replaced`
        // (OnDownloadFixture.cs:69-77).
        let mut req = request(NotificationEventType::ImportComplete);
        req.file = Some(deleted_update("/media/TV/Example Show/old.mkv"));
        assert!(library_plan(&req).clean);
    }

    #[test]
    fn an_import_flagged_as_an_upgrade_cleans_even_without_media_updates() {
        let mut req = request(NotificationEventType::ImportComplete);
        req.import = Some(PluginNotificationImport {
            upgrade: true,
            ..PluginNotificationImport::default()
        });
        assert!(library_plan(&req).clean);
    }

    #[test]
    fn replaced_paths_from_the_contract_also_mean_a_clean() {
        let mut req = request(NotificationEventType::ImportComplete);
        req.import = Some(PluginNotificationImport {
            replaced_paths: vec!["/media/TV/Example Show/old.mkv".to_string()],
            ..PluginNotificationImport::default()
        });
        assert!(library_plan(&req).clean);
    }

    #[test]
    fn an_upgrade_always_scans_and_cleans() {
        assert_eq!(
            library_plan(&request(NotificationEventType::Upgrade)),
            LibraryPlan {
                notify: true,
                scan: true,
                clean: true
            }
        );
    }

    #[test]
    fn rename_updates_and_cleans_without_notifying() {
        // `OnRename` calls `UpdateAndClean` only (`Xbmc.cs:50-53`).
        assert_eq!(
            library_plan(&request(NotificationEventType::Rename)),
            LibraryPlan {
                notify: false,
                scan: true,
                clean: true
            }
        );
    }

    #[test]
    fn a_file_delete_notifies_scans_and_cleans() {
        for event_type in [
            NotificationEventType::FileDeleted,
            NotificationEventType::FileDeletedForUpgrade,
        ] {
            assert_eq!(
                library_plan(&request(event_type)),
                LibraryPlan {
                    notify: true,
                    scan: true,
                    clean: true
                },
                "{event_type:?}"
            );
        }
    }

    #[test]
    fn a_title_add_notifies_scans_and_cleans() {
        assert_eq!(
            library_plan(&request(NotificationEventType::TitleAdded)),
            LibraryPlan {
                notify: true,
                scan: true,
                clean: true
            }
        );
    }

    #[test]
    fn a_title_delete_cleans_only_when_it_carries_deleted_paths() {
        // Sonarr acts at all only when `deleteMessage.DeletedFiles`
        // (`Xbmc.cs:70-79`); the contract has no such flag.
        let plan = library_plan(&request(NotificationEventType::TitleDeleted));
        assert!(plan.scan);
        assert!(!plan.clean);

        let mut req = request(NotificationEventType::TitleDeleted);
        req.file = Some(deleted_update("/media/TV/Example Show/s01e01.mkv"));
        assert!(library_plan(&req).clean);
    }

    #[test]
    fn every_other_event_notifies_only() {
        for event_type in [
            NotificationEventType::ImportRejected,
            NotificationEventType::PostProcessingCompleted,
            NotificationEventType::SubtitleDownloaded,
            NotificationEventType::SubtitleSearchFailed,
            NotificationEventType::MediaRequestSubmitted,
            NotificationEventType::MediaRequestApproved,
            NotificationEventType::MediaRequestRejected,
            NotificationEventType::MediaRequestCanceled,
            NotificationEventType::HealthIssue,
            NotificationEventType::HealthRestored,
            NotificationEventType::ApplicationUpdate,
            NotificationEventType::ManualInteractionRequired,
            NotificationEventType::Test,
        ] {
            let plan = library_plan(&request(event_type));
            assert!(plan.notify, "{event_type:?}");
            assert!(!plan.scan, "{event_type:?}");
            assert!(!plan.clean, "{event_type:?}");
        }
    }

    // -----------------------------------------------------------------------
    // The channel's own switches
    // -----------------------------------------------------------------------

    #[test]
    fn the_three_switches_narrow_the_plan() {
        let mut req = request(NotificationEventType::Upgrade);
        req.file = Some(deleted_update("/media/TV/Example Show/old.mkv"));

        let all_off = Settings {
            notify: false,
            update_library: false,
            clean_library: false,
            ..settings()
        };
        assert_eq!(effective_plan(&req, &all_off), LibraryPlan::default());

        let notify_only = Settings {
            update_library: false,
            clean_library: false,
            ..settings()
        };
        assert_eq!(
            effective_plan(&req, &notify_only),
            LibraryPlan {
                notify: true,
                scan: false,
                clean: false
            }
        );

        let library_only = Settings {
            notify: false,
            ..settings()
        };
        assert_eq!(
            effective_plan(&req, &library_only),
            LibraryPlan {
                notify: false,
                scan: true,
                clean: true
            }
        );
    }

    #[test]
    fn a_connection_test_notifies_even_with_gui_notifications_off() {
        // `XbmcService.Test` calls `Notify` directly, bypassing
        // `Settings.Notify` (`XbmcService.cs:127-140`). The June port did not,
        // so a library-only channel tested green without touching Kodi.
        let req = request(NotificationEventType::Test);
        assert!(req.is_test);
        let library_only = Settings {
            notify: false,
            ..settings()
        };
        assert!(effective_plan(&req, &library_only).notify);
    }

    // -----------------------------------------------------------------------
    // Message
    // -----------------------------------------------------------------------

    #[test]
    fn headers_keep_sonarrs_branded_set() {
        let mut req = request(NotificationEventType::Grab);
        assert_eq!(notification_header(&req), "Scryer - Grabbed");
        req.event_type = NotificationEventType::ImportComplete;
        assert_eq!(notification_header(&req), "Scryer - Imported");
        req.event_type = NotificationEventType::TitleAdded;
        assert_eq!(notification_header(&req), "Scryer - Added");
        req.event_type = NotificationEventType::FileDeleted;
        assert_eq!(notification_header(&req), "Scryer - Deleted");
    }

    #[test]
    fn the_download_header_says_it_failed() {
        let req = request(NotificationEventType::Download);
        assert_eq!(notification_header(&req), "Scryer - Download Failed");
    }

    #[test]
    fn the_header_uses_the_applications_own_name() {
        let mut req = request(NotificationEventType::Grab);
        req.app.name = "Scryer (staging)".to_string();
        assert_eq!(notification_header(&req), "Scryer (staging) - Grabbed");
    }

    #[test]
    fn the_message_is_the_summary_with_the_quality_appended_once() {
        let mut req = request(NotificationEventType::ImportComplete);
        req.release = Some(PluginNotificationRelease {
            quality: Some("WEBDL-1080p".to_string()),
            ..PluginNotificationRelease::default()
        });
        let mut warnings = Vec::new();
        assert_eq!(
            notification_message(&req, &mut warnings),
            "Imported 1 file for 'Example Show'. (WEBDL-1080p)"
        );

        req.summary_message = "Imported WEBDL-1080p for 'Example Show'.".to_string();
        let mut warnings = Vec::new();
        assert_eq!(
            notification_message(&req, &mut warnings),
            "Imported WEBDL-1080p for 'Example Show'."
        );
    }

    #[test]
    fn newlines_are_collapsed_and_long_text_is_truncated_with_a_warning() {
        let mut req = request(NotificationEventType::HealthIssue);
        req.summary_message = format!("line one\nline two\n{}", "x".repeat(400));
        let mut warnings = Vec::new();
        let message = notification_message(&req, &mut warnings);
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS);
        assert!(message.ends_with('…'));
        assert!(!message.contains('\n'));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn an_empty_summary_falls_back_to_the_heading() {
        let mut req = request(NotificationEventType::Rename);
        req.summary_message = "   ".to_string();
        req.summary_title = "Renamed: Example Show".to_string();
        let mut warnings = Vec::new();
        assert_eq!(
            notification_message(&req, &mut warnings),
            "Renamed: Example Show"
        );
    }

    // -----------------------------------------------------------------------
    // Image
    // -----------------------------------------------------------------------

    #[test]
    fn the_image_is_kodis_own_badge_and_never_a_dead_url() {
        let mut req = request(NotificationEventType::Grab);
        req.severity = Some(NotificationSeverity::Warning);
        assert_eq!(notification_image(&req, &settings()), "warning");

        req.severity = None;
        req.event_type = NotificationEventType::Download;
        assert_eq!(notification_image(&req, &settings()), "error");

        req.event_type = NotificationEventType::Grab;
        assert_eq!(notification_image(&req, &settings()), "info");
    }

    #[test]
    fn a_poster_is_only_used_when_the_operator_opted_in() {
        let mut req = request(NotificationEventType::Grab);
        req.title.as_mut().expect("title").poster_url =
            Some("https://images.example/poster.jpg".to_string());

        assert_eq!(notification_image(&req, &settings()), "info");

        let opted_in = Settings {
            notification_poster: true,
            ..settings()
        };
        assert_eq!(
            notification_image(&req, &opted_in),
            "https://images.example/poster.jpg"
        );
    }

    #[test]
    fn a_relative_poster_path_is_not_sent_to_kodi() {
        let mut req = request(NotificationEventType::Grab);
        req.title.as_mut().expect("title").poster_url = Some("/posters/1.jpg".to_string());
        let opted_in = Settings {
            notification_poster: true,
            ..settings()
        };
        assert_eq!(notification_image(&req, &opted_in), "info");
    }

    #[test]
    fn show_notification_params_match_the_kodi_schema() {
        let req = request(NotificationEventType::Grab);
        let params = show_notification_params(&req, &settings(), "hello");
        assert_eq!(params["title"], "Scryer - Grabbed");
        assert_eq!(params["message"], "hello");
        assert_eq!(params["image"], "info");
        assert_eq!(params["displaytime"], 5000);
    }

    #[test]
    fn a_display_time_below_kodis_own_minimum_is_clamped_in_the_payload() {
        let req = request(NotificationEventType::Grab);
        let settings = Settings {
            display_time_ms: 500,
            ..settings()
        };
        let params = show_notification_params(&req, &settings, "hello");
        assert_eq!(params["displaytime"], KODI_MIN_DISPLAY_TIME_MS);
    }

    // -----------------------------------------------------------------------
    // Versions
    // -----------------------------------------------------------------------

    #[test]
    fn the_version_object_parses_and_gates_the_clean_parameters() {
        let result = json!({"version": {"major": 13, "minor": 5, "patch": 0}});
        let parsed = parse_jsonrpc_version(&result).expect("parsed");
        assert_eq!(parsed, version(13, 5));
        assert!(parsed.supports_uniqueid());
        assert!(parsed.supports_clean_content());
        assert!(parsed.supports_clean_directory());
    }

    #[test]
    fn the_gates_match_the_kodi_release_each_parameter_arrived_in() {
        // Krypton 8.0.0, Leia 10.3.0, Matrix 12.4.0, Nexus 13.0.0, Omega 13.5.0.
        assert!(version(8, 0).supports_uniqueid());
        assert!(!version(6, 0).supports_uniqueid());
        assert!(!version(8, 0).supports_clean_content());
        assert!(version(10, 3).supports_clean_content());
        assert!(!version(10, 3).supports_clean_directory());
        assert!(version(12, 4).supports_clean_directory());
    }

    #[test]
    fn a_pre_frodo_integer_version_is_still_understood() {
        let parsed = parse_jsonrpc_version(&json!({"version": 4})).expect("parsed");
        assert_eq!(parsed.major, 4);
        assert!(!parsed.supports_uniqueid());
    }

    #[test]
    fn a_response_without_a_version_is_not_a_version() {
        assert!(parse_jsonrpc_version(&json!({})).is_none());
        assert!(parse_jsonrpc_version(&Value::Null).is_none());
    }

    #[test]
    fn an_old_kodi_is_reported_at_test_time() {
        let warnings = version_warnings(version(8, 0));
        assert!(warnings.iter().any(|warning| warning.contains("8.0.0")));
        assert!(warnings.iter().any(|warning| warning.contains("Leia")));
    }

    // -----------------------------------------------------------------------
    // Scan and clean parameters
    // -----------------------------------------------------------------------

    #[test]
    fn a_scan_without_a_directory_omits_it_and_suppresses_the_dialog() {
        let params = scan_params(None, &settings());
        assert!(params.get("directory").is_none());
        assert_eq!(params["showdialogs"], false);
    }

    #[test]
    fn a_scan_with_a_directory_scopes_it() {
        let params = scan_params(Some("/media/TV/Example Show/"), &settings());
        assert_eq!(params["directory"], "/media/TV/Example Show/");
    }

    #[test]
    fn clean_scopes_to_the_facet_and_folder_only_where_kodi_supports_it() {
        let directory = Some("/media/TV/Example Show/");

        // Krypton: neither parameter exists.
        let params = clean_params(Facet::Series, directory, Some(version(8, 0)), &settings());
        assert!(params.get("content").is_none());
        assert!(params.get("directory").is_none());

        // Leia: content only.
        let params = clean_params(Facet::Series, directory, Some(version(10, 3)), &settings());
        assert_eq!(params["content"], "tvshows");
        assert!(params.get("directory").is_none());

        // Matrix and later: both.
        let params = clean_params(Facet::Movie, directory, Some(version(12, 4)), &settings());
        assert_eq!(params["content"], "movies");
        assert_eq!(params["directory"], "/media/TV/Example Show/");
    }

    #[test]
    fn an_unknown_version_falls_back_to_sonarrs_unscoped_clean() {
        let params = clean_params(Facet::Series, Some("/media/TV/X/"), None, &settings());
        assert!(params.get("content").is_none());
        assert!(params.get("directory").is_none());
        assert_eq!(params["showdialogs"], false);
    }

    #[test]
    fn an_ambiguous_facet_keeps_the_clean_unscoped_by_content() {
        let params = clean_params(Facet::Ambiguous, None, Some(version(13, 5)), &settings());
        assert!(params.get("content").is_none());
    }

    // -----------------------------------------------------------------------
    // Facets and lookups
    // -----------------------------------------------------------------------

    #[test]
    fn facets_map_to_the_half_of_the_library_they_live_in() {
        assert_eq!(facet_of(Some(&title("X", "movie"))), Facet::Movie);
        assert_eq!(facet_of(Some(&title("X", "series"))), Facet::Series);
        assert_eq!(facet_of(Some(&title("X", "anime"))), Facet::Ambiguous);
        // Older fixtures say `tv`, and an unknown facet is a series, which is
        // what the June port assumed for everything.
        assert_eq!(facet_of(Some(&title("X", "tv"))), Facet::Series);
        assert_eq!(facet_of(None), Facet::Series);
    }

    #[test]
    fn lookup_properties_ask_for_uniqueid_only_where_it_exists() {
        assert!(lookup_properties(Some(version(8, 0)), Facet::Series).contains(&"uniqueid"));
        assert!(!lookup_properties(Some(version(6, 0)), Facet::Series).contains(&"uniqueid"));
        // An unknown version asks for it: an unknown property is ignored by
        // Kodi, while omitting it loses the match.
        assert!(lookup_properties(None, Facet::Series).contains(&"uniqueid"));
        assert!(lookup_properties(None, Facet::Movie).contains(&"year"));
        assert!(!lookup_properties(None, Facet::Series).contains(&"year"));
    }

    #[test]
    fn a_tv_show_matches_on_the_tvdb_uniqueid_before_its_label() {
        let shows = vec![
            json!({"label": "Example Show", "file": "/wrong/", "uniqueid": {"tvdb": "111"}}),
            json!({"label": "Other", "file": "/media/TV/Example Show/", "uniqueid": {"tvdb": "999"}}),
        ];
        let mut title = title("Example Show", "series");
        title.external_ids.tvdb_id = Some("999".to_string());
        assert_eq!(
            match_tv_show(&shows, &title).as_deref(),
            Some("/media/TV/Example Show/")
        );
    }

    #[test]
    fn a_tv_show_still_matches_sonarrs_tvdb_in_imdbnumber_quirk() {
        // `int.TryParse(s.ImdbNumber, out var tvdbId); tvdbId == series.TvdbId`
        // (`XbmcService.cs:73-75`).
        let shows = vec![json!({
            "label": "Example Show",
            "file": "/media/TV/Example Show/",
            "imdbnumber": "999"
        })];
        let mut title = title("Different Name", "series");
        title.external_ids.tvdb_id = Some("999".to_string());
        assert_eq!(
            match_tv_show(&shows, &title).as_deref(),
            Some("/media/TV/Example Show/")
        );
    }

    #[test]
    fn a_tv_show_falls_back_to_the_label_case_insensitively() {
        let shows = vec![json!({"label": "example show", "file": "/media/TV/Example Show/"})];
        let title = title("Example Show", "series");
        assert_eq!(
            match_tv_show(&shows, &title).as_deref(),
            Some("/media/TV/Example Show/")
        );
    }

    #[test]
    fn an_unmatched_tv_show_yields_nothing_so_the_whole_library_is_scanned() {
        let shows = vec![json!({"label": "Something Else", "file": "/media/TV/Else/"})];
        assert!(match_tv_show(&shows, &title("Example Show", "series")).is_none());
    }

    #[test]
    fn a_movie_matches_on_tmdb_then_imdb_then_label_and_year() {
        let movies = vec![
            json!({"label": "Example", "year": 1999, "file": "/media/Movies/Example (1999)/e.mkv", "uniqueid": {"tmdb": "42", "imdb": "tt0011"}}),
            json!({"label": "Example", "year": 2020, "file": "/media/Movies/Example (2020)/e.mkv", "uniqueid": {"tmdb": "43"}}),
        ];

        let mut by_tmdb = title("Example", "movie");
        by_tmdb.external_ids.tmdb_id = Some("43".to_string());
        assert_eq!(
            match_movie(&movies, &by_tmdb).as_deref(),
            Some("/media/Movies/Example (2020)/e.mkv")
        );

        let mut by_imdb = title("Example", "movie");
        // Kodi stores the `tt` prefix; Scryer's id may not.
        by_imdb.external_ids.imdb_id = Some("0011".to_string());
        assert_eq!(
            match_movie(&movies, &by_imdb).as_deref(),
            Some("/media/Movies/Example (1999)/e.mkv")
        );

        let mut by_label = title("Example", "movie");
        by_label.year = Some(2020);
        assert_eq!(
            match_movie(&movies, &by_label).as_deref(),
            Some("/media/Movies/Example (2020)/e.mkv")
        );
    }

    #[test]
    fn a_movie_label_match_is_refused_when_the_years_disagree() {
        let movies = vec![json!({"label": "Example", "year": 1999, "file": "/media/Movies/e.mkv"})];
        let mut title = title("Example", "movie");
        title.year = Some(2020);
        assert!(match_movie(&movies, &title).is_none());
    }

    #[test]
    fn a_numeric_uniqueid_is_compared_as_a_number() {
        let shows = vec![json!({"label": "X", "file": "/x/", "uniqueid": {"tvdb": 999}})];
        let mut title = title("Y", "series");
        title.external_ids.tvdb_id = Some("999".to_string());
        assert_eq!(match_tv_show(&shows, &title).as_deref(), Some("/x/"));
    }

    #[test]
    fn a_movie_file_resolves_to_the_folder_kodi_can_scan() {
        assert_eq!(
            parent_directory("/media/Movies/Example (2020)/e.mkv").as_deref(),
            Some("/media/Movies/Example (2020)/")
        );
        assert_eq!(
            parent_directory("smb://nas/Movies/Example/e.mkv").as_deref(),
            Some("smb://nas/Movies/Example/")
        );
        assert_eq!(
            parent_directory(r"C:\Movies\Example\e.mkv").as_deref(),
            Some(r"C:\Movies\Example\")
        );
        // Already a folder.
        assert_eq!(
            parent_directory("/media/TV/Example Show/").as_deref(),
            Some("/media/TV/Example Show/")
        );
    }

    #[test]
    fn kodis_virtual_paths_are_not_directories() {
        assert!(parent_directory("stack:///a/1.mkv , /a/2.mkv").is_none());
        assert!(parent_directory("multipath://%2fa%2f").is_none());
        assert!(parent_directory("rar:///a/x.rar/x.mkv").is_none());
        assert!(parent_directory("smb://nas").is_none());
        assert!(parent_directory("   ").is_none());
    }

    // -----------------------------------------------------------------------
    // Active players
    // -----------------------------------------------------------------------

    #[test]
    fn only_a_video_player_blocks_library_work() {
        assert!(has_active_video_player(
            &json!([{"playerid": 1, "type": "video", "playertype": "internal"}])
        ));
        assert!(!has_active_video_player(
            &json!([{"playerid": 0, "type": "audio", "playertype": "internal"}])
        ));
        assert!(!has_active_video_player(&json!([])));
        assert!(!has_active_video_player(&Value::Null));
        // An unrecognised player type is not a video player.
        assert!(!has_active_video_player(&json!([{"type": "hologram"}])));
    }

    // -----------------------------------------------------------------------
    // Response classification
    // -----------------------------------------------------------------------

    fn classify(status: u16, body: &str) -> CallOutcome {
        classify_response(
            status,
            None,
            body.as_bytes(),
            "GUI.ShowNotification",
            &settings(),
        )
    }

    fn expect_error(outcome: CallOutcome) -> PluginError {
        match outcome {
            CallOutcome::Misconfigured(error) => error,
            CallOutcome::Ok(_) => panic!("expected a typed error, got a success"),
            CallOutcome::Rejected { detail, .. } => {
                panic!("expected a typed error, got a rejection: {detail}")
            }
        }
    }

    #[test]
    fn a_successful_call_returns_kodis_result() {
        let outcome = classify(200, r#"{"jsonrpc":"2.0","id":1,"result":"OK"}"#);
        assert_eq!(outcome.result(), Some(&json!("OK")));
        assert!(outcome.failure().is_none());
    }

    #[test]
    fn a_401_names_the_credentials() {
        let error = expect_error(classify(401, "<html>Unauthorized</html>"));
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("username"));
        assert!(error.public_message.contains("password"));
    }

    #[test]
    fn a_403_is_also_an_auth_failure() {
        assert_eq!(
            expect_error(classify(403, "nope")).code,
            PluginErrorCode::AuthFailed
        );
    }

    #[test]
    fn a_404_names_url_base() {
        let error = expect_error(classify(404, "<html>Not Found</html>"));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("url_base"));
    }

    #[test]
    fn an_html_page_on_a_200_names_url_base_rather_than_the_credentials() {
        let error = expect_error(classify(200, "<!DOCTYPE html><title>Kodi</title>"));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("url_base"));
    }

    #[test]
    fn an_empty_body_is_not_valid_json_rpc() {
        // `"Invalid response from XBMC, the response is not valid JSON"`
        // (`XbmcJsonApiProxy.cs:102`).
        let error = expect_error(classify(200, ""));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn a_json_rpc_error_keeps_sonarrs_wording_and_names_the_method() {
        let error = expect_error(classify(
            200,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found."}}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert_eq!(
            error.public_message,
            "Kodi JSON error. Code = -32601, Message: Method not found. (method GUI.ShowNotification)"
        );
    }

    #[test]
    fn a_json_rpc_error_with_structured_data_still_classifies() {
        // Sonarr's `ErrorResult.Error` is a `Dictionary<string, string>` and
        // cannot deserialise this at all (`Model/ErrorResult.cs`).
        let error = expect_error(classify(
            200,
            r#"{"error":{"code":-32602,"message":"Invalid params.","data":{"method":"GUI.ShowNotification","stack":{"message":"Invalid type"}}}}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(error.public_message.contains("Code = -32602"));
    }

    #[test]
    fn a_500_is_a_delivery_failure_carrying_retry_after() {
        let outcome = classify_response(
            503,
            Some(30),
            b"upstream exploded",
            "VideoLibrary.Scan",
            &settings(),
        );
        let CallOutcome::Rejected {
            provider_status,
            retry_after_seconds,
            ..
        } = outcome
        else {
            panic!("a 503 must stay on the delivery lane");
        };
        assert_eq!(provider_status, "http_503");
        assert_eq!(retry_after_seconds, Some(30));
    }

    #[test]
    fn a_refused_egress_names_server_url() {
        let outcome = transport_failure(
            "HTTP request to http://kodi.local:8080/jsonrpc is not allowed",
            "GUI.ShowNotification",
            &settings(),
            false,
        );
        let error = expect_error(outcome);
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("server_url"));
        assert!(error.public_message.contains("allowlist"));
    }

    #[test]
    fn an_unreachable_kodi_is_typed_only_on_a_connection_test() {
        let strict = transport_failure("connection refused", "JSONRPC.Version", &settings(), true);
        assert_eq!(
            expect_error(strict).code,
            PluginErrorCode::UpstreamUnavailable
        );

        let lenient = transport_failure(
            "connection refused",
            "GUI.ShowNotification",
            &settings(),
            false,
        );
        assert!(matches!(lenient, CallOutcome::Rejected { .. }));
    }

    #[test]
    fn a_long_html_body_is_bounded_before_it_reaches_the_operator() {
        let error = expect_error(classify(400, &"<p>boom</p>".repeat(200)));
        assert!(error.public_message.chars().count() < 600);
    }

    #[test]
    fn retry_after_reads_the_header_case_insensitively() {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("Retry-After".to_string(), "12".to_string());
        assert_eq!(retry_after(&headers), Some(12));
        headers.insert("Retry-After".to_string(), "not a number".to_string());
        assert_eq!(retry_after(&headers), None);
    }

    // -----------------------------------------------------------------------
    // Aggregation
    // -----------------------------------------------------------------------

    fn misconfigured(message: &str) -> Call {
        Call {
            method: "GUI.ShowNotification",
            outcome: CallOutcome::Misconfigured(plugin_error(
                PluginErrorCode::AuthFailed,
                message.to_string(),
                None,
            )),
        }
    }

    #[test]
    fn every_call_failing_the_same_way_is_a_typed_error() {
        let calls = vec![
            misconfigured("credentials rejected"),
            Call {
                method: "VideoLibrary.Scan",
                outcome: CallOutcome::Misconfigured(plugin_error(
                    PluginErrorCode::AuthFailed,
                    "credentials rejected".to_string(),
                    None,
                )),
            },
        ];
        let PluginResult::Err(error) = finish(calls, Vec::new()) else {
            panic!("expected a typed error");
        };
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
    }

    #[test]
    fn a_partial_failure_stays_on_the_delivery_lane_and_reports_each_method() {
        let calls = vec![
            Call {
                method: "GUI.ShowNotification",
                outcome: CallOutcome::Ok(json!("OK")),
            },
            Call {
                method: "VideoLibrary.Clean",
                outcome: CallOutcome::Rejected {
                    detail: "Kodi answered HTTP 503".to_string(),
                    provider_status: "http_503".to_string(),
                    retry_after_seconds: Some(5),
                },
            },
        ];
        let PluginResult::Ok(response) = finish(calls, vec!["a warning".to_string()]) else {
            panic!("expected a delivery result");
        };
        assert!(!response.success);
        assert_eq!(response.retry_after_seconds, Some(5));
        assert_eq!(response.target_results.len(), 2);
        assert!(response.target_results[0].success);
        assert!(!response.target_results[1].success);
        assert_eq!(response.target_results[1].target, "VideoLibrary.Clean");
        assert_eq!(response.warnings, vec!["a warning".to_string()]);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("VideoLibrary.Clean"))
        );
    }

    #[test]
    fn all_calls_succeeding_is_a_success_with_the_warnings_kept() {
        let calls = vec![Call {
            method: "GUI.ShowNotification",
            outcome: CallOutcome::Ok(json!("OK")),
        }];
        let PluginResult::Ok(response) = finish(calls, vec!["heads up".to_string()]) else {
            panic!("expected a delivery result");
        };
        assert!(response.success);
        assert_eq!(response.warnings, vec!["heads up".to_string()]);
    }
}
