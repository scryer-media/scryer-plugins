//! Pushbullet pushes, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Pushbullet notification (`src/NzbDrone.Core/Notifications/PushBullet/`)
//! form-posts one note per channel tag, else one per device, else one to the
//! whole account, and throws a single "unable to send to all channels or
//! devices" if any of them failed (`PushBulletProxy.cs`). Its `Test` blames a
//! 401 on the API key and says nothing useful about anything else, and a live
//! send only logs.
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * every push goes out as the documented JSON body with the `Access-Token`
//!   header ("All POST requests must use a JSON body with the Content-Type
//!   header set to application/json");
//! * each channel or device is its own push and reports its own
//!   `target_results` entry, so one bad device no longer hides behind "one or
//!   more targets failed";
//! * failures are classified from Pushbullet's error JSON
//!   (`{"error":{"type","message","param","cat"}}`): a rejected token is
//!   `AuthFailed` on `api_key`, a rejected device or channel is `InvalidConfig`
//!   on `device_ids` / `channel_tags`, a 429 is a delivery failure carrying
//!   `retry_after_seconds` from `X-Ratelimit-Reset`, and a 5xx is a retryable
//!   delivery failure;
//! * the free-account monthly push quota is recognised and reported as such
//!   (typed `RateLimited` with an actionable message) instead of as a generic
//!   rejection, and the remaining targets are skipped rather than each failing
//!   the same way;
//! * a Test resolves the configured device idens against the account's active
//!   devices, so a nickname or a stale id is caught before the first real event.
//!
//! # Upstream reference
//!
//! Read 2026-09-29, <https://docs.pushbullet.com/> (last changed June 2020): the
//! authentication section, "Requests" (JSON bodies), "Errors" (400/401/403/404/
//! 429/5XX and the error object), "Ratelimiting" (`X-Ratelimit-Limit`,
//! `X-Ratelimit-Remaining`, `X-Ratelimit-Reset` as a Unix timestamp), "Push
//! Limit" ("Free accounts … are limited to 500 pushes per month. Going over will
//! result in an error when sending a Push."), create-push (one target per push;
//! no target broadcasts to every device) and list-devices. The docs do not name
//! the quota error. The live API has answered it since August 2016 with the
//! error code `pushbullet_pro_required` and the message "Pushbullet Pro is
//! required to make this call.", which is what is matched here.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, PluginNotificationEpisode,
    PluginNotificationTargetResult, current_sdk_constraint,
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

const PROVIDER_TYPE: &str = "pushbullet";
const USER_AGENT: &str = concat!("scryer-pushbullet-plugin/", env!("CARGO_PKG_VERSION"));

const PUSHBULLET_API_BASE: &str = "https://api.pushbullet.com";
const PUSHBULLET_API_HOST: &str = "api.pushbullet.com";
const PUSHES_URL: &str = "https://api.pushbullet.com/v2/pushes";
const DEVICES_URL: &str = "https://api.pushbullet.com/v2/devices";
const TOKEN_SETTINGS_URL: &str = "https://www.pushbullet.com/#settings/account";

/// "Free accounts (without a Pro subscription) are limited to 500 pushes per
/// month."
const FREE_MONTHLY_PUSHES: u32 = 500;
/// The error code the live API returns once that quota is spent. Not in the
/// docs; observed on the wire since the limit was introduced in August 2016.
const QUOTA_ERROR_CODE: &str = "pushbullet_pro_required";
const QUOTA_ERROR_MESSAGE: &str = "pushbullet pro is required";

/// `GET /v2/devices` is cursor-paginated. An account with more devices than
/// this many pages hold is not verified exhaustively, and the Test says so.
const DEVICE_PAGE_LIMIT: usize = 5;

/// The action Sonarr's settings page calls to fill its device picker, kept
/// under the same name.
const GET_DEVICES_ACTION: &str = "getDevices";

const METADATA_LINK_AUTO: &str = "auto";
const METADATA_LINK_NONE: &str = "none";

/// Which site a link push should open. Sonarr's Pushbullet sends notes only;
/// `auto` picks the best id the title actually carries for its facet.
const METADATA_LINK_OPTIONS: &[(&str, &str)] = &[
    (METADATA_LINK_AUTO, "Automatic"),
    (METADATA_LINK_NONE, "None"),
    ("imdb", "IMDb"),
    ("tvdb", "TVDb"),
    ("tvmaze", "TVMaze"),
    ("trakt", "Trakt"),
    ("tmdb", "TMDb"),
    ("anidb", "AniDB"),
    ("anilist", "AniList"),
    ("mal", "MyAnimeList"),
    ("kitsu", "Kitsu"),
];

const AUTO_LINK_EPISODIC: &[&str] = &["tvdb", "tvmaze", "imdb", "tmdb", "anidb"];
const AUTO_LINK_OTHER: &[&str] = &["tmdb", "imdb", "tvdb"];

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Pushbullet".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            // Fixed by the product: every call is against api.pushbullet.com.
            default_base_url: Some(PUSHBULLET_API_BASE.to_string()),
            allowed_hosts: vec![PUSHBULLET_API_HOST.to_string()],
            capabilities: NotificationCapabilities {
                // Pushes are plain text on every Pushbullet client.
                supports_rich_text: false,
                // A file push needs an upload through `/v2/upload-request` to a
                // storage host `allowed_hosts` cannot name, and every image
                // push would count against the free monthly quota.
                supports_images: false,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![NotificationDeliveryMode::Push],
                payload_formats: vec![NotificationPayloadFormat::PlainText],
                supported_events: general_notification_events(),
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
        field(
            "api_key",
            "Access Token",
            ConfigFieldType::Password,
            true,
            None,
            Some(
                "A Pushbullet access token, from the Access Tokens section of https://www.pushbullet.com/#settings/account.",
            ),
        ),
        // Sonarr models both lists as tag inputs (`PushBulletSettings.cs`).
        // Scryer renders a `Tag` field as comma-separated text, so values
        // saved by earlier versions of this plugin load unchanged.
        field(
            "device_ids",
            "Device IDs",
            ConfigFieldType::Tag,
            false,
            None,
            Some(
                "Device idens to push to, one push per device. Leave this and Channel Tags empty to push to every device on the account. A Test lists the account's devices if one is not found.",
            ),
        ),
        field(
            "channel_tags",
            "Channel Tags",
            ConfigFieldType::Tag,
            false,
            None,
            Some(
                "Tags of channels owned by this account, one push per channel. When set, Device IDs is ignored.",
            ),
        ),
        field(
            "sender_id",
            "Sender ID",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Optional iden of the device the pushes are sent from. Leave empty to send from the account.",
            ),
        ),
        field(
            "include_app_name_in_title",
            "Include App Name In Title",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Prefixes the push title with the Scryer application name."),
        ),
        select_field(
            "metadata_link",
            "Metadata Link",
            Some(METADATA_LINK_AUTO),
            METADATA_LINK_OPTIONS,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    access_token: String,
    channels: Vec<String>,
    devices: Vec<String>,
    sender: Option<String>,
    include_app_name_in_title: bool,
    metadata_link: String,
}

impl Settings {
    /// Resolved from a key lookup rather than from the host directly, so the
    /// parsing of values saved by earlier versions can be tested without one.
    ///
    /// Every key is the one 0.1.x and 0.2.0 stored, and the lists still accept
    /// commas, semicolons and newlines as separators.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, PluginError> {
        let value = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let access_token = value("api_key").ok_or_else(|| {
            plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "api_key is not configured: paste a Pushbullet access token from {TOKEN_SETTINGS_URL}"
                ),
                None,
            )
        })?;

        let include_app_name_in_title = value("include_app_name_in_title")
            .map(|raw| {
                matches!(
                    raw.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

        Ok(Self {
            access_token,
            channels: split_list(value("channel_tags").as_deref()),
            devices: split_list(value("device_ids").as_deref()),
            sender: value("sender_id"),
            include_app_name_in_title,
            metadata_link: validated_metadata_link(value("metadata_link").as_deref())?,
        })
    }
}

fn split_list(raw: Option<&str>) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    for part in raw.unwrap_or_default().split([',', ';', '\n', '\r']) {
        let part = part.trim();
        if !part.is_empty() && !values.iter().any(|existing| existing == part) {
            values.push(part.to_string());
        }
    }
    values
}

fn validated_metadata_link(raw: Option<&str>) -> Result<String, PluginError> {
    let value = raw
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| METADATA_LINK_AUTO.to_string());
    if !METADATA_LINK_OPTIONS.iter().any(|(key, _)| *key == value) {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("metadata_link is not a valid value: {value}"),
            Some(format!(
                "known values: {}",
                METADATA_LINK_OPTIONS
                    .iter()
                    .map(|(key, _)| *key)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        ));
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// "Each push has a target, if you don't specify a target, we will broadcast it
/// to all of the user's devices. Only one target may be specified." One target
/// per push is therefore not a choice: two channels are two pushes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Channel(String),
    Device(String),
    Everyone,
}

impl Target {
    fn label(&self) -> String {
        match self {
            Target::Channel(tag) => format!("channel:{tag}"),
            Target::Device(iden) => format!("device:{iden}"),
            Target::Everyone => "all devices".to_string(),
        }
    }
}

/// Channels win over devices, and neither means the whole account. That is
/// Sonarr's precedence (`PushBulletProxy.cs`), which saved configurations rely
/// on.
///
/// Every device entry is sent as `device_iden`. Sonarr and the 0.1.x plugin
/// sent an all-digit value as `device_id`, a parameter the API does not
/// document. The live API ignores it and sends the push to every device, so
/// a targeted push would silently become a broadcast.
fn targets(settings: &Settings) -> Vec<Target> {
    if !settings.channels.is_empty() {
        return settings
            .channels
            .iter()
            .cloned()
            .map(Target::Channel)
            .collect();
    }
    if !settings.devices.is_empty() {
        return settings
            .devices
            .iter()
            .cloned()
            .map(Target::Device)
            .collect();
    }
    vec![Target::Everyone]
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Scryer's dispatcher already composes an event-specific heading in
/// `summary_title` ("Grabbed: Example Show"), which is what a lock screen should
/// show. Sonarr's "Sonarr - …" branding is available as an opt-in prefix.
fn heading(req: &PluginNotificationRequest, settings: &Settings) -> String {
    let app = req.app.name.trim();
    let title = req.summary_title.trim();
    let base = if title.is_empty() {
        if app.is_empty() { "Scryer" } else { app }
    } else {
        title
    };
    if settings.include_app_name_in_title && !app.is_empty() && base != app {
        format!("{app} - {base}")
    } else {
        base.to_string()
    }
}

fn body(req: &PluginNotificationRequest, settings: &Settings) -> String {
    let mut lines = Vec::new();
    let message = req.summary_message.trim();
    if !message.is_empty() {
        lines.push(message.to_string());
    }
    lines.extend(detail_lines(req));
    if lines.is_empty() {
        lines.push(heading(req, settings));
    }
    lines.join("\n")
}

/// The structured enrichment Sonarr's one-sentence message has no room for.
/// Every line is conditional on its block being present, so the sparse request
/// the core sends today renders just the summary.
fn detail_lines(req: &PluginNotificationRequest) -> Vec<String> {
    let summary = req.summary_message.as_str();
    let mut lines = Vec::new();
    match req.event_type {
        NotificationEventType::Grab => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Release Group", release_group(req));
            push(&mut lines, "Indexer", indexer(req));
            push(&mut lines, "Size", size(req));
            push(&mut lines, "Client", client_name(req));
        }
        // The dispatcher maps a failed download onto `Download`; a successful
        // import is `ImportComplete`/`Upgrade`.
        NotificationEventType::Download => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Client", client_name(req));
            push(&mut lines, "Status", download_status(req));
        }
        NotificationEventType::ImportComplete
        | NotificationEventType::Upgrade
        | NotificationEventType::PostProcessingCompleted => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Quality", quality(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Release Group", release_group(req));
            push(&mut lines, "Size", size(req));
            push(&mut lines, "Client", client_name(req));
            push_path(&mut lines, "Destination", import_path(req), summary);
        }
        NotificationEventType::ImportRejected => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Release", release_title(req));
            push_path(&mut lines, "Source", source_path(req), summary);
            push(&mut lines, "Status", import_status(req));
        }
        NotificationEventType::Rename => {
            push(&mut lines, "Episode", episode_display(req));
            push_path(&mut lines, "File", primary_path(req), summary);
        }
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            push(&mut lines, "Episode", episode_display(req));
            push_path(&mut lines, "File", deleted_path(req), summary);
            push(&mut lines, "Quality", quality(req));
        }
        NotificationEventType::TitleAdded | NotificationEventType::TitleDeleted => {
            push_path(&mut lines, "Path", title_path(req), summary);
        }
        NotificationEventType::HealthIssue | NotificationEventType::HealthRestored => {
            push(&mut lines, "Check", health_source(req));
            push(&mut lines, "Detail", health_detail(req));
        }
        NotificationEventType::ApplicationUpdate => {
            push(
                &mut lines,
                "Previous Version",
                application_version(req, false),
            );
            push(&mut lines, "New Version", application_version(req, true));
        }
        NotificationEventType::ManualInteractionRequired => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Download", download_title(req));
            push(&mut lines, "Client", client_name(req));
            push(&mut lines, "Reason", manual_reason(req));
        }
        NotificationEventType::SubtitleDownloaded | NotificationEventType::SubtitleSearchFailed => {
            push(&mut lines, "Episode", episode_display(req));
            push_path(&mut lines, "File", primary_path(req), summary);
            push(&mut lines, "Languages", subtitle_languages(req));
        }
        NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestApproved
        | NotificationEventType::MediaRequestRejected
        | NotificationEventType::MediaRequestCanceled => {
            push(&mut lines, "Status", media_request_status(req));
            push(&mut lines, "Quality Profile", media_request_profile(req));
        }
        NotificationEventType::TitleMoved => {
            push_path(&mut lines, "From", title_move_source(req), summary);
            push_path(&mut lines, "To", title_move_destination(req), summary);
            push(&mut lines, "Warning", title_move_warning(req));
        }
        // Not in `supported_events`, so the host never routes them here; the
        // summary lines are all such a notification would carry.
        NotificationEventType::ListTitleAdded
        | NotificationEventType::ListRequestSubmitted
        | NotificationEventType::ListItemHeld
        | NotificationEventType::ListTitleLeft
        | NotificationEventType::ListSyncFailed
        | NotificationEventType::ListUnfollowed
        | NotificationEventType::Test => {}
    }
    lines
}

fn push(lines: &mut Vec<String>, label: &str, value: Option<String>) {
    if let Some(value) = non_empty(value) {
        lines.push(format!("{label}: {value}"));
    }
}

/// A path the summary already names is not repeated: the dispatcher writes the
/// deleted file's path and a moved title's destination into `summary_message`,
/// and a phone shows only the first few lines of a push. Only the path lines
/// (and the library a move falls back to) are matched this way, so a short
/// value such as a quality is never dropped for appearing inside a longer word.
fn push_path(lines: &mut Vec<String>, label: &str, value: Option<String>, summary: &str) {
    if let Some(value) = non_empty(value)
        && !summary.contains(value.as_str())
    {
        lines.push(format!("{label}: {value}"));
    }
}

// ---------------------------------------------------------------------------
// Field values
// ---------------------------------------------------------------------------

fn episode_display(req: &PluginNotificationRequest) -> Option<String> {
    if let Some(display) = req
        .episode
        .as_ref()
        .and_then(|episode| episode.display.as_deref())
        .map(str::trim)
        .filter(|display| !display.is_empty())
    {
        return Some(display.to_string());
    }

    let episodes: Vec<&PluginNotificationEpisode> = if req.episodes.is_empty() {
        req.episode.iter().collect()
    } else {
        req.episodes.iter().collect()
    };
    let first = episodes.first().copied()?;

    let titles = episodes
        .iter()
        .filter_map(|episode| episode.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .collect::<Vec<_>>()
        .join(" + ");

    if first.episode_number.is_none()
        && let Some(air_date) = first
            .air_date
            .as_deref()
            .map(str::trim)
            .filter(|air_date| !air_date.is_empty())
    {
        return Some(if titles.is_empty() {
            air_date.to_string()
        } else {
            format!("{air_date} - {titles}")
        });
    }

    let numbers: String = episodes
        .iter()
        .filter_map(|episode| episode.episode_number.as_deref())
        .map(str::trim)
        .filter(|number| !number.is_empty())
        .map(|number| match number.parse::<u32>() {
            Ok(parsed) => format!("x{parsed:02}"),
            Err(_) => format!("x{number}"),
        })
        .collect();

    let season = first
        .season_number
        .as_deref()
        .map(str::trim)
        .filter(|season| !season.is_empty());

    match (season, numbers.is_empty(), titles.is_empty()) {
        (Some(season), false, true) => Some(format!("{season}{numbers}")),
        (Some(season), false, false) => Some(format!("{season}{numbers} - {titles}")),
        (_, _, false) => Some(titles),
        _ => None,
    }
}

fn quality(req: &PluginNotificationRequest) -> Option<String> {
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

fn release_title(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.source_title.clone()),
    )
    .or_else(|| {
        non_empty(
            req.import
                .as_ref()
                .and_then(|import| import.source_title.clone()),
        )
    })
    .or_else(|| download_title(req))
}

fn release_group(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.release_group.clone()),
    )
    .or_else(|| {
        req.media_files
            .iter()
            .find_map(|file| non_empty(file.release_group.clone()))
    })
}

fn indexer(req: &PluginNotificationRequest) -> Option<String> {
    let release = req.release.as_ref()?;
    non_empty(release.indexer.clone()).or_else(|| non_empty(release.provider.clone()))
}

fn size(req: &PluginNotificationRequest) -> Option<String> {
    let bytes = req
        .download
        .as_ref()
        .and_then(|download| download.size_bytes)
        .filter(|bytes| *bytes > 0)
        .or_else(|| {
            let total: i64 = req
                .media_files
                .iter()
                .filter_map(|file| file.size_bytes)
                .sum();
            (total > 0).then_some(total)
        })?;
    Some(format_bytes(bytes))
}

fn client_name(req: &PluginNotificationRequest) -> Option<String> {
    let download = req.download.as_ref()?;
    non_empty(download.client_name.clone()).or_else(|| non_empty(download.client_type.clone()))
}

fn download_title(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.download
            .as_ref()
            .and_then(|download| download.title.clone()),
    )
}

fn download_status(req: &PluginNotificationRequest) -> Option<String> {
    let download = req.download.as_ref()?;
    non_empty(download.status_message.clone()).or_else(|| non_empty(download.status.clone()))
}

fn import_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.import
            .as_ref()
            .and_then(|import| import.dest_path.clone()),
    )
    .or_else(|| primary_path(req))
}

fn source_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.import
            .as_ref()
            .and_then(|import| import.source_path.clone()),
    )
}

fn import_status(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.import.as_ref().and_then(|import| import.status.clone()))
}

fn primary_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.file.as_ref().and_then(|file| file.primary_path.clone())).or_else(|| {
        req.file.as_ref().and_then(|file| {
            file.media_updates
                .first()
                .map(|update| update.path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
    })
}

fn deleted_path(req: &PluginNotificationRequest) -> Option<String> {
    req.file
        .as_ref()
        .and_then(|file| {
            file.media_updates
                .iter()
                .find(|update| {
                    update.update_type == scryer_plugin_sdk::NotificationMediaUpdateType::Deleted
                })
                .map(|update| update.path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
        .or_else(|| {
            req.import.as_ref().and_then(|import| {
                import
                    .deleted_paths
                    .first()
                    .map(|path| path.trim().to_string())
                    .filter(|path| !path.is_empty())
            })
        })
        .or_else(|| primary_path(req))
}

fn title_path(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(req.title.as_ref().and_then(|title| title.path.clone()))
}

fn health_source(req: &PluginNotificationRequest) -> Option<String> {
    let health = req.health.as_ref()?;
    non_empty(health.code.clone()).or_else(|| non_empty(health.status.clone()))
}

fn health_detail(req: &PluginNotificationRequest) -> Option<String> {
    let health = req.health.as_ref()?;
    non_empty(health.details.clone()).or_else(|| non_empty(health.message.clone()))
}

fn application_version(req: &PluginNotificationRequest, target: bool) -> Option<String> {
    let update = req.application_update.as_ref()?;
    non_empty(if target {
        update.target_version.clone()
    } else {
        update.current_version.clone()
    })
}

fn manual_reason(req: &PluginNotificationRequest) -> Option<String> {
    let interaction = req.manual_interaction.as_ref()?;
    non_empty(interaction.reason.clone()).or_else(|| non_empty(interaction.kind.clone()))
}

/// Only an absolute http(s) link becomes a link push: a relative path opens
/// nothing on a phone.
fn manual_link(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.manual_interaction
            .as_ref()
            .and_then(|interaction| interaction.link.clone()),
    )
    .filter(|link| {
        let link = link.to_ascii_lowercase();
        link.starts_with("http://") || link.starts_with("https://")
    })
}

fn subtitle_languages(req: &PluginNotificationRequest) -> Option<String> {
    let languages: Vec<String> = req
        .media_files
        .iter()
        .flat_map(|file| file.subtitle_languages.iter())
        .map(|language| language.trim().to_string())
        .filter(|language| !language.is_empty())
        .collect();
    (!languages.is_empty()).then(|| languages.join(", "))
}

fn media_request_status(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.media_request
            .as_ref()
            .and_then(|request| request.status.clone()),
    )
}

fn media_request_profile(req: &PluginNotificationRequest) -> Option<String> {
    let request = req.media_request.as_ref()?;
    non_empty(request.approved_quality_profile_name.clone())
        .or_else(|| non_empty(request.requested_quality_profile_name.clone()))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Sonarr's `BytesToString` rounding, so sizes read the same across channels.
fn format_bytes(bytes: i64) -> String {
    const SUFFIXES: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let magnitude = bytes.unsigned_abs() as f64;
    let place = (magnitude.log(1024.0).floor() as i32).clamp(0, 6);
    let scaled = magnitude / 1024f64.powi(place);
    let rounded = (scaled * 10.0).round() / 10.0;
    let signed = if bytes < 0 { -rounded } else { rounded };
    format!("{} {}", signed, SUFFIXES[place as usize])
}

// ---------------------------------------------------------------------------
// Link
// ---------------------------------------------------------------------------

/// A link push opens its `url` when tapped. `ManualInteractionRequired` carries
/// its own deep link into Scryer and wins; otherwise the chosen metadata site
/// (or the best id the title carries, under `auto`) opens the title. Sonarr's
/// Pushbullet only ever sends notes.
fn push_link(req: &PluginNotificationRequest, settings: &Settings) -> Option<String> {
    if let Some(link) = manual_link(req) {
        return Some(link);
    }
    if settings.metadata_link == METADATA_LINK_NONE {
        return None;
    }
    metadata_link(req, &settings.metadata_link)
}

fn metadata_link(req: &PluginNotificationRequest, choice: &str) -> Option<String> {
    let title = req.title.as_ref()?;
    let ids = &title.external_ids;
    let episodic = matches!(
        title.facet.to_ascii_lowercase().as_str(),
        "series" | "anime" | "tv" | "show"
    );

    if choice == METADATA_LINK_AUTO {
        let order = if episodic {
            AUTO_LINK_EPISODIC
        } else {
            AUTO_LINK_OTHER
        };
        return order.iter().find_map(|key| link_for(key, ids, episodic));
    }

    link_for(choice, ids, episodic)
}

fn link_for(
    key: &str,
    ids: &scryer_plugin_sdk::PluginNotificationExternalIds,
    episodic: bool,
) -> Option<String> {
    let imdb = external_id(ids.imdb_id.as_deref(), ids, "imdb");
    let tvdb = external_id(ids.tvdb_id.as_deref(), ids, "tvdb");
    let tmdb = external_id(ids.tmdb_id.as_deref(), ids, "tmdb");
    let tvmaze = external_id(ids.tvmaze_id.as_deref(), ids, "tvmaze");

    match key {
        "imdb" => imdb.map(|id| format!("https://www.imdb.com/title/{id}")),
        "tvdb" => tvdb.map(|id| format!("https://thetvdb.com/?tab=series&id={id}")),
        "tvmaze" => tvmaze.map(|id| format!("https://www.tvmaze.com/shows/{id}")),
        "trakt" => {
            if episodic {
                tvdb.map(|id| format!("https://trakt.tv/search/tvdb/{id}?id_type=show"))
            } else {
                tmdb.map(|id| format!("https://trakt.tv/search/tmdb/{id}?id_type=movie"))
                    .or_else(|| imdb.map(|id| format!("https://trakt.tv/search/imdb/{id}")))
            }
        }
        "tmdb" => tmdb.map(|id| {
            if episodic {
                format!("https://www.themoviedb.org/tv/{id}")
            } else {
                format!("https://www.themoviedb.org/movie/{id}")
            }
        }),
        "anidb" => external_id(ids.anidb_id.as_deref(), ids, "anidb")
            .map(|id| format!("https://anidb.net/anime/{id}")),
        "anilist" => external_id(ids.anilist_ids.first().map(String::as_str), ids, "anilist")
            .map(|id| format!("https://anilist.co/anime/{id}")),
        "mal" => external_id(ids.mal_ids.first().map(String::as_str), ids, "mal")
            .map(|id| format!("https://myanimelist.net/anime/{id}")),
        "kitsu" => external_id(ids.kitsu_ids.first().map(String::as_str), ids, "kitsu")
            .map(|id| format!("https://kitsu.app/anime/{id}")),
        _ => None,
    }
}

fn external_id(
    typed: Option<&str>,
    ids: &scryer_plugin_sdk::PluginNotificationExternalIds,
    source: &str,
) -> Option<String> {
    typed
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or_else(|| {
            ids.by_source
                .get(source)
                .and_then(|values| values.first())
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
        })
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// The JSON body of one `POST /v2/pushes`.
fn push_payload(req: &PluginNotificationRequest, settings: &Settings, target: &Target) -> Value {
    let mut payload = Map::new();
    let link = push_link(req, settings);
    let kind = if link.is_some() { "link" } else { "note" };
    payload.insert("type".to_string(), Value::String(kind.to_string()));
    payload.insert("title".to_string(), Value::String(heading(req, settings)));
    payload.insert("body".to_string(), Value::String(body(req, settings)));
    if let Some(link) = link {
        payload.insert("url".to_string(), Value::String(link));
    }
    match target {
        Target::Channel(tag) => {
            payload.insert("channel_tag".to_string(), Value::String(tag.clone()));
        }
        Target::Device(iden) => {
            payload.insert("device_iden".to_string(), Value::String(iden.clone()));
        }
        Target::Everyone => {}
    }
    if let Some(sender) = &settings.sender {
        payload.insert(
            "source_device_iden".to_string(),
            Value::String(sender.clone()),
        );
    }
    if let Some(guid) = push_guid(req, target) {
        payload.insert("guid".to_string(), Value::String(guid));
    }
    Value::Object(payload)
}

/// Pushbullet documents that "pushes with guid set are mostly idempotent":
/// a second push with the same guid returns the first instead of creating
/// another. Scryer re-sends a disk-space notification to a channel until the
/// whole send succeeds, so without a guid one failing device would re-deliver
/// the event to every device that already had it on each retry.
///
/// The guid is derived from the event and the target, so a retry of the same
/// event to the same target repeats it while each device or channel of one
/// event, and every other event, gets its own. A Test has no event id and
/// sends none.
fn push_guid(req: &PluginNotificationRequest, target: &Target) -> Option<String> {
    let event_id = non_empty(req.event_id.clone())?;
    // FNV-1a: stable across builds and platforms, unlike `DefaultHasher`.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in event_id
        .bytes()
        .chain(std::iter::once(0))
        .chain(target.label().bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("scryer-{hash:016x}"))
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One request this plugin asks the host to make. The send and action paths
/// take the transport as a closure, so every classification below is exercised
/// by unit tests against scripted replies instead of a live API.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outbound {
    method: &'static str,
    url: String,
    headers: Vec<(&'static str, String)>,
    body: Option<Vec<u8>>,
}

impl Outbound {
    fn new(method: &'static str, url: String, token: &str) -> Self {
        Self {
            method,
            url,
            headers: vec![
                // The documented credential header. Sonarr sends the token as
                // a Basic username instead, which the docs also accept; one
                // plain header keeps the secret in one obvious place.
                ("Access-Token", token.to_string()),
                ("Accept", "application/json".to_string()),
                ("User-Agent", USER_AGENT.to_string()),
            ],
            body: None,
        }
    }

    fn json(mut self, payload: &Value) -> Self {
        self.headers
            .push(("Content-Type", "application/json".to_string()));
        self.body = Some(serde_json::to_vec(payload).unwrap_or_default());
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

type Transport<'a> = dyn FnMut(&Outbound) -> Result<Reply, String> + 'a;

fn host_transport(outbound: &Outbound) -> Result<Reply, String> {
    let mut request = HttpRequest::new(outbound.url.clone()).with_method(outbound.method);
    for (name, value) in &outbound.headers {
        request = request.with_header(*name, value.clone());
    }
    match http::request::<Vec<u8>>(&request, outbound.body.clone()) {
        Ok(response) => Ok(Reply {
            status: response.status_code(),
            headers: response.headers().clone(),
            body: response.body(),
        }),
        Err(error) => Err(error.to_string()),
    }
}

fn now_unix() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// Pushbullet's error object
// ---------------------------------------------------------------------------

/// `{"error":{"type":"invalid_request","message":"…","param":"…","cat":"…"}}`.
///
/// `code` is not documented, but the live API puts it on every error (and
/// repeats it as a top-level `error_code`). It is the only field that names the
/// monthly quota.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ApiError {
    code: Option<String>,
    message: Option<String>,
    param: Option<String>,
    raw: String,
}

impl ApiError {
    fn parse(body: &[u8]) -> Self {
        let raw = String::from_utf8_lossy(body).trim().to_string();
        let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
            return Self {
                raw,
                ..Self::default()
            };
        };
        let error = map.get("error");
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Self {
            code: text(error.and_then(|error| error.get("code")))
                .or_else(|| text(map.get("error_code"))),
            message: text(error.and_then(|error| error.get("message"))),
            param: text(error.and_then(|error| error.get("param"))),
            raw,
        }
    }

    fn detail(&self, status: u16) -> String {
        if let Some(message) = &self.message {
            return message.clone();
        }
        if self.raw.is_empty() {
            return format!("HTTP {status}");
        }
        let mut raw: String = self.raw.chars().take(300).collect();
        if raw.len() < self.raw.len() {
            raw.push('…');
        }
        raw
    }

    fn is_monthly_quota(&self) -> bool {
        self.code.as_deref() == Some(QUOTA_ERROR_CODE)
            || self
                .message
                .as_deref()
                .is_some_and(|message| message.to_ascii_lowercase().contains(QUOTA_ERROR_MESSAGE))
    }

    fn mentions(&self, needle: &str) -> bool {
        self.param.as_deref() == Some(needle)
            || self
                .message
                .as_deref()
                .is_some_and(|message| message.to_ascii_lowercase().contains(needle))
    }
}

fn quota_error(detail: &str) -> PluginError {
    plugin_error(
        PluginErrorCode::RateLimited,
        format!(
            "Pushbullet refused the push because it needs Pushbullet Pro. For a note or link push that is the free monthly push limit: accounts without Pro may send {FREE_MONTHLY_PUSHES} pushes a month, and pushes work again when the limit resets or the account is upgraded ({detail})"
        ),
        Some(format!("{QUOTA_ERROR_CODE}: {detail}")),
    )
}

fn token_rejected(detail: &str) -> PluginError {
    plugin_error(
        PluginErrorCode::AuthFailed,
        format!(
            "api_key was rejected by Pushbullet ({detail}). Create a new access token at {TOKEN_SETTINGS_URL}."
        ),
        None,
    )
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// What one push produced.
#[derive(Debug)]
enum Outcome {
    Delivered {
        iden: Option<String>,
        headers: BTreeMap<String, String>,
    },
    /// A retryable delivery failure for this target only.
    Failed {
        status: String,
        message: String,
        retry_after: Option<i64>,
    },
    /// This target's setting is wrong; the others may still be fine.
    Misconfigured(PluginError),
    /// Wrong for every target (token, quota, sender): stop sending.
    Fatal(PluginError),
    /// A delivery failure that would repeat for every target (429, host
    /// unreachable): stop sending.
    Halt {
        status: String,
        message: String,
        retry_after: Option<i64>,
    },
}

fn classify_push(target: &Target, reply: &Reply, now: Option<i64>) -> Outcome {
    let status = reply.status;
    if (200..300).contains(&status) {
        let iden = serde_json::from_slice::<Value>(&reply.body)
            .ok()
            .and_then(|body| body.get("iden").and_then(Value::as_str).map(str::to_string));
        return Outcome::Delivered {
            iden,
            headers: reply.headers.clone(),
        };
    }

    let answer = ApiError::parse(&reply.body);
    let detail = answer.detail(status);

    // Checked before the status: the docs do not say which status the quota
    // error carries, only that "going over will result in an error".
    if answer.is_monthly_quota() {
        return Outcome::Fatal(quota_error(&detail));
    }

    match status {
        // "No valid access token provided".
        401 => Outcome::Fatal(token_rejected(&detail)),
        // "The access token is not valid for that request". The live API
        // answers an unowned channel with a 400 naming `channel_tag` (handled
        // below); a 403 on a channel push is still read as the channel, and
        // anywhere else it is the token.
        403 => match target {
            Target::Channel(tag) => Outcome::Misconfigured(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "channel_tags: Pushbullet refused channel {tag:?} ({detail}). The channel must exist and be owned by the account this access token belongs to."
                ),
                None,
            )),
            _ => Outcome::Fatal(plugin_error(
                PluginErrorCode::AuthFailed,
                format!(
                    "api_key is not allowed to create this push ({detail}). Check the access token at {TOKEN_SETTINGS_URL}."
                ),
                None,
            )),
        },
        429 => Outcome::Halt {
            status: "http_429".to_string(),
            message: format!("Pushbullet rate limit reached: {detail}"),
            retry_after: retry_after(&reply.headers, now),
        },
        500..=599 => Outcome::Failed {
            status: format!("http_{status}"),
            message: format!("HTTP {status}: {detail}"),
            retry_after: retry_after_header(&reply.headers),
        },
        _ => classify_rejection(target, status, &answer, &detail),
    }
}

/// A 400, 404 or other 4xx: attribute it to the setting that produced the
/// offending parameter when Pushbullet names one.
fn classify_rejection(target: &Target, status: u16, answer: &ApiError, detail: &str) -> Outcome {
    if answer.mentions("source_device_iden") {
        return Outcome::Fatal(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "sender_id was rejected by Pushbullet ({detail}). Use a device iden from this account, or leave it empty."
            ),
            None,
        ));
    }

    let rejected = match target {
        Target::Channel(tag)
            if answer.mentions("channel_tag") || answer.mentions("channel") || status == 404 =>
        {
            Some((
                "channel_tags",
                tag,
                "The channel must exist and be owned by this account.",
            ))
        }
        Target::Device(iden)
            if answer.mentions("device_iden") || answer.mentions("device") || status == 404 =>
        {
            Some((
                "device_ids",
                iden,
                "Run a Test to list this account's devices and their idens.",
            ))
        }
        _ => None,
    };
    if let Some((setting, value, hint)) = rejected {
        return Outcome::Misconfigured(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("{setting}: Pushbullet rejected {value:?} ({detail}). {hint}"),
            Some(format!("HTTP {status}")),
        ));
    }

    Outcome::Misconfigured(plugin_error(
        PluginErrorCode::Permanent,
        format!("Pushbullet rejected the push this plugin built (HTTP {status}): {detail}"),
        None,
    ))
}

/// A proxy's `Retry-After` is already a delay; `X-Ratelimit-Reset` is "the unix
/// timestamp in seconds when the rate limit will be reset", so it becomes one.
fn retry_after(headers: &BTreeMap<String, String>, now: Option<i64>) -> Option<i64> {
    if let Some(seconds) = retry_after_header(headers) {
        return Some(seconds);
    }
    let reset = header(headers, "x-ratelimit-reset")?.parse::<i64>().ok()?;
    Some((reset - now?).max(1))
}

fn retry_after_header(headers: &BTreeMap<String, String>) -> Option<i64> {
    header(headers, "retry-after")
        .and_then(|value| value.parse::<i64>().ok())
        .map(|seconds| seconds.max(1))
}

/// The request rate limit, in Pushbullet's cost units (a request is 1, a
/// database operation 4). It is not the monthly push quota, which the API does
/// not expose before it is exceeded.
fn rate_limit_warning(headers: &BTreeMap<String, String>) -> Option<String> {
    let remaining = header(headers, "x-ratelimit-remaining")?
        .parse::<i64>()
        .ok()?;
    let limit = header(headers, "x-ratelimit-limit")?.parse::<i64>().ok()?;
    (limit > 0 && remaining * 20 <= limit).then(|| {
        format!(
            "Pushbullet's request rate limit is nearly exhausted: {remaining} of {limit} units remain in the current window"
        )
    })
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

fn code_status(code: PluginErrorCode) -> &'static str {
    match code {
        PluginErrorCode::InvalidConfig => "invalid_config",
        PluginErrorCode::AuthFailed => "auth_failed",
        PluginErrorCode::RateLimited => "rate_limited",
        PluginErrorCode::UpstreamUnavailable => "upstream_unavailable",
        PluginErrorCode::Unsupported => "unsupported",
        PluginErrorCode::Temporary => "temporary",
        PluginErrorCode::Permanent => "permanent",
    }
}

fn target_result(
    target: &Target,
    success: bool,
    status: &str,
    error: Option<String>,
) -> PluginNotificationTargetResult {
    PluginNotificationTargetResult {
        target: target.label(),
        success,
        status: Some(status.to_string()),
        error,
    }
}

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let settings = match Settings::from_lookup(config_value) {
        Ok(settings) => settings,
        Err(error) => return PluginResult::Err(error),
    };
    deliver(req, &settings, &mut host_transport, now_unix())
}

fn deliver(
    req: &PluginNotificationRequest,
    settings: &Settings,
    transport: &mut Transport<'_>,
    now: Option<i64>,
) -> PluginResult<PluginNotificationResponse> {
    let mut warnings = Vec::new();
    if req.is_test
        && let Err(error) = verify_configuration(settings, transport, &mut warnings)
    {
        return PluginResult::Err(error);
    }

    let targets = targets(settings);
    let mut results = Vec::with_capacity(targets.len());
    let mut delivered: Vec<Option<String>> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut statuses: Vec<String> = Vec::new();
    let mut misconfigured: Vec<PluginError> = Vec::new();
    let mut fatal: Option<PluginError> = None;
    let mut retry: Option<i64> = None;
    let mut rate_warning: Option<String> = None;
    let mut stopped_at: Option<usize> = None;

    for (index, target) in targets.iter().enumerate() {
        let outbound = Outbound::new("POST", PUSHES_URL.to_string(), &settings.access_token)
            .json(&push_payload(req, settings, target));
        let outcome = match transport(&outbound) {
            Ok(reply) => classify_push(target, &reply, now),
            // The host answers a refused or failed egress in-band. On a live
            // send that is the provider being unreachable: a delivery failure
            // the core may retry. On a Test the operator is waiting for an
            // answer, and "Pushbullet is unreachable" is that answer.
            Err(error) if req.is_test => Outcome::Fatal(plugin_error(
                PluginErrorCode::UpstreamUnavailable,
                format!("Pushbullet could not be reached: {error}"),
                None,
            )),
            Err(error) => Outcome::Halt {
                status: "request_failed".to_string(),
                message: format!("request failed: {error}"),
                retry_after: None,
            },
        };

        match outcome {
            Outcome::Delivered { iden, headers } => {
                results.push(target_result(target, true, "delivered", None));
                delivered.push(iden);
                if let Some(warning) = rate_limit_warning(&headers) {
                    rate_warning = Some(warning);
                }
            }
            Outcome::Failed {
                status,
                message,
                retry_after,
            } => {
                results.push(target_result(target, false, &status, Some(message.clone())));
                failures.push(format!("{}: {message}", target.label()));
                statuses.push(status);
                retry = retry.max(retry_after);
            }
            Outcome::Misconfigured(error) => {
                results.push(target_result(
                    target,
                    false,
                    code_status(error.code),
                    Some(error.public_message.clone()),
                ));
                misconfigured.push(error);
            }
            Outcome::Fatal(error) => {
                results.push(target_result(
                    target,
                    false,
                    code_status(error.code),
                    Some(error.public_message.clone()),
                ));
                fatal = Some(error);
                stopped_at = Some(index);
                break;
            }
            Outcome::Halt {
                status,
                message,
                retry_after,
            } => {
                results.push(target_result(target, false, &status, Some(message.clone())));
                failures.push(format!("{}: {message}", target.label()));
                statuses.push(status);
                retry = retry.max(retry_after);
                stopped_at = Some(index);
                break;
            }
        }
    }

    // Targets after a stop are reported, not silently dropped.
    if let Some(index) = stopped_at {
        for target in &targets[index + 1..] {
            results.push(target_result(
                target,
                false,
                "not_attempted",
                Some("not attempted: an earlier push failed in a way every push would".to_string()),
            ));
        }
    }
    warnings.extend(rate_warning);

    if let Some(error) = fatal {
        if delivered.is_empty() {
            return PluginResult::Err(error);
        }
        // Some pushes already went out, so the event was partly delivered, and
        // a typed error would say nothing was.
        let mut response = error_response(
            error.public_message,
            Some(code_status(error.code).to_string()),
        );
        response.warnings = warnings;
        response.target_results = results;
        return PluginResult::Ok(response);
    }

    if delivered.is_empty() && failures.is_empty() && !misconfigured.is_empty() {
        // Nothing went out and every target was refused for its configuration:
        // that is a configuration error, not a delivery failure.
        let first = misconfigured[0].code;
        let code = if misconfigured.iter().all(|error| error.code == first) {
            first
        } else {
            PluginErrorCode::InvalidConfig
        };
        let mut messages: Vec<String> = Vec::new();
        for error in &misconfigured {
            if !messages.contains(&error.public_message) {
                messages.push(error.public_message.clone());
            }
        }
        return PluginResult::Err(plugin_error(code, messages.join("; "), None));
    }

    if failures.is_empty() && misconfigured.is_empty() {
        let mut response = ok_response();
        if let [Some(iden)] = delivered.as_slice() {
            response.delivery_id = Some(iden.clone());
        }
        response.warnings = warnings;
        if targets.len() > 1 {
            response.target_results = results;
        }
        return PluginResult::Ok(response);
    }

    let mut messages = failures;
    messages.extend(
        misconfigured
            .iter()
            .map(|error| error.public_message.clone()),
    );
    let failed = results.iter().filter(|result| !result.success).count();
    let status = match statuses.as_slice() {
        [single] if targets.len() == 1 => single.clone(),
        _ => format!("{failed}/{} targets failed", targets.len()),
    };
    let mut response = error_response(messages.join("; "), Some(status));
    response.retry_after_seconds = retry;
    response.warnings = warnings;
    response.target_results = results;
    PluginResult::Ok(response)
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Device {
    iden: String,
    nickname: Option<String>,
}

impl Device {
    fn describe(&self) -> String {
        match &self.nickname {
            Some(nickname) => format!("{nickname} ({})", self.iden),
            None => self.iden.clone(),
        }
    }
}

#[derive(Debug)]
enum DeviceListError {
    Unreachable(String),
    Unauthorized(String),
    Quota(String),
    Rejected(u16, String),
}

/// `GET /v2/devices?active=true`, following `cursor` for up to
/// [`DEVICE_PAGE_LIMIT`] pages. The `bool` is whether the list is complete.
fn list_devices(
    token: &str,
    transport: &mut Transport<'_>,
) -> Result<(Vec<Device>, bool), DeviceListError> {
    let mut devices = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..DEVICE_PAGE_LIMIT {
        let mut query = vec![("active", "true".to_string())];
        if let Some(cursor) = &cursor {
            query.push(("cursor", cursor.clone()));
        }
        let outbound = Outbound::new("GET", append_query(DEVICES_URL, &query), token);
        let reply = transport(&outbound).map_err(DeviceListError::Unreachable)?;
        if !(200..300).contains(&reply.status) {
            let answer = ApiError::parse(&reply.body);
            let detail = answer.detail(reply.status);
            return Err(if answer.is_monthly_quota() {
                DeviceListError::Quota(detail)
            } else if matches!(reply.status, 401 | 403) {
                DeviceListError::Unauthorized(detail)
            } else {
                DeviceListError::Rejected(reply.status, detail)
            });
        }
        let body: Value = serde_json::from_slice(&reply.body).map_err(|error| {
            DeviceListError::Rejected(reply.status, format!("unreadable device list: {error}"))
        })?;
        for device in body
            .get("devices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // `active: false` is a deleted device.
            if device.get("active").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let Some(iden) = device
                .get("iden")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|iden| !iden.is_empty())
            else {
                continue;
            };
            devices.push(Device {
                iden: iden.to_string(),
                nickname: device
                    .get("nickname")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|nickname| !nickname.is_empty())
                    .map(str::to_string),
            });
        }
        cursor = body
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_string);
        if cursor.is_none() {
            return Ok((devices, true));
        }
    }
    Ok((devices, false))
}

/// The Test-time checks a live send cannot afford: resolve every configured
/// device iden against the account, so a nickname, a numeric id from an older
/// setup, or a deleted device is named before the first real event is lost.
fn verify_configuration(
    settings: &Settings,
    transport: &mut Transport<'_>,
    warnings: &mut Vec<String>,
) -> Result<(), PluginError> {
    let uses_devices = settings.channels.is_empty() && !settings.devices.is_empty();
    if !settings.channels.is_empty() && !settings.devices.is_empty() {
        warnings.push(
            "channel_tags is set, so device_ids is ignored: Pushbullet allows one target per push and channels take priority".to_string(),
        );
    }
    if !uses_devices && settings.sender.is_none() {
        return Ok(());
    }

    let (devices, complete) = match list_devices(&settings.access_token, transport) {
        Ok(listed) => listed,
        Err(DeviceListError::Unauthorized(detail)) => return Err(token_rejected(&detail)),
        Err(DeviceListError::Quota(detail)) => return Err(quota_error(&detail)),
        Err(DeviceListError::Unreachable(detail)) | Err(DeviceListError::Rejected(_, detail)) => {
            warnings.push(format!(
                "could not list this account's devices to check device_ids and sender_id: {detail}"
            ));
            return Ok(());
        }
    };
    let known = |iden: &str| devices.iter().any(|device| device.iden == iden);

    if uses_devices {
        let mut problems = Vec::new();
        for configured in &settings.devices {
            if known(configured) {
                continue;
            }
            match devices.iter().find(|device| {
                device
                    .nickname
                    .as_deref()
                    .is_some_and(|nickname| nickname.eq_ignore_ascii_case(configured))
            }) {
                Some(device) => problems.push(format!(
                    "{configured:?} is a device nickname; use its iden {:?}",
                    device.iden
                )),
                None => problems.push(format!(
                    "{configured:?} is not an active device on this account"
                )),
            }
        }
        if !problems.is_empty() && !complete {
            warnings.push(format!(
                "device_ids may be wrong ({}), but the account has more devices than were checked",
                problems.join("; ")
            ));
        } else if !problems.is_empty() {
            let available = if devices.is_empty() {
                "none".to_string()
            } else {
                devices
                    .iter()
                    .map(Device::describe)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "device_ids: {}. Devices on this account: {available}",
                    problems.join("; ")
                ),
                None,
            ));
        }
    }

    if let Some(sender) = &settings.sender
        && complete
        && !known(sender)
    {
        warnings.push(format!(
            "sender_id {sender:?} is not an active device on this account; Pushbullet may reject pushes that name it"
        ));
    }
    Ok(())
}

/// `getDevices`, the action Sonarr's settings page calls to fill its device
/// picker. Scryer's SDK cannot point a field's options at an action, so nothing
/// in the core calls this today; it is kept, answering in the `{id, name}`
/// options shape, for when something can.
fn device_options(
    token: Option<String>,
    transport: &mut Transport<'_>,
) -> PluginResult<PluginActionResponse> {
    let Some(token) = token else {
        return PluginResult::Ok(PluginActionResponse {
            payload: json!({ "options": [] }),
        });
    };
    let devices = match list_devices(&token, transport) {
        Ok((devices, _)) => devices,
        Err(DeviceListError::Unauthorized(detail)) => {
            return PluginResult::Err(token_rejected(&detail));
        }
        Err(DeviceListError::Quota(detail)) => return PluginResult::Err(quota_error(&detail)),
        Err(DeviceListError::Unreachable(detail)) => {
            return PluginResult::Err(plugin_error(
                PluginErrorCode::UpstreamUnavailable,
                format!("Pushbullet could not be reached: {detail}"),
                None,
            ));
        }
        Err(DeviceListError::Rejected(status, detail)) => {
            let code = if status == 429 || status >= 500 {
                PluginErrorCode::Temporary
            } else {
                PluginErrorCode::Permanent
            };
            return PluginResult::Err(plugin_error(
                code,
                format!("Pushbullet devices request failed (HTTP {status}): {detail}"),
                None,
            ));
        }
    };

    // Sonarr lists named devices only, sorted by name ignoring case.
    let mut named: Vec<(String, String)> = devices
        .into_iter()
        .filter_map(|device| Some((device.nickname?, device.iden)))
        .collect();
    named.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then_with(|| left.1.cmp(&right.1))
    });
    let options: Vec<Value> = named
        .into_iter()
        .map(|(name, id)| json!({ "id": id, "name": name }))
        .collect();
    PluginResult::Ok(PluginActionResponse {
        payload: json!({ "options": options }),
    })
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

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum. `getDevices` is answered; any other action is answered
/// in-band as `Unsupported` rather than trapping, which under a component would
/// cost the whole instance.
fn handle_notification_command(
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    match command {
        PluginNotificationCommand::Send(request) => {
            PluginNotificationCommandResult::Send(send_notification(&request))
        }
        PluginNotificationCommand::Action(request) if request.action == GET_DEVICES_ACTION => {
            PluginNotificationCommandResult::Action(device_options(
                config_value("api_key"),
                &mut host_transport,
            ))
        }
        PluginNotificationCommand::Action(_) => {
            PluginNotificationCommandResult::Action(unsupported_action(PROVIDER_TYPE))
        }
    }
}

#[cfg(test)]
mod tests;
