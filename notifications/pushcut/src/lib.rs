//! Pushcut smart notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Pushcut notification (`src/NzbDrone.Core/Notifications/Pushcut/`) is
//! a JSON POST to `https://api.pushcut.io/v1/notifications/{notificationName}`
//! carrying four fields — `title`, `text`, `image`, `isTimeSensitive` — plus one
//! action per selected metadata link (`PushcutProxy.cs:39-65`). Every failure is
//! an exception that is only ever caught inside `Test`
//! (`PushcutProxy.cs:78-107`), and that catch tests for `403`
//! (`PushcutProxy.cs:88`) — which is not the status Pushcut returns for a bad
//! key. A live send just throws into the log.
//!
//! The June port copied that shape onto `notify_common::send_json`, so every
//! upstream answer collapsed into `HTTP N: body` and every configuration
//! problem became a `FnResult` fault.
//!
//! This module rebuilds the channel on Scryer's notification contract and on
//! Pushcut's current OpenAPI document:
//!
//! * a rejected key is `AuthFailed` naming `api_key`, an undefined notification
//!   is `InvalidConfig` naming `notification_name`, a payload Pushcut refuses is
//!   `Permanent`, and a quota or server fault is a *delivery* failure carrying
//!   `provider_status`/`retry_after_seconds` — the two lanes the notification
//!   adapter distinguishes;
//! * a `Test` resolves the configured notification (and device) names against
//!   the account through `GET /v1/notifications` and `GET /v1/devices` before it
//!   sends, so the operator is told "there is no notification called X; the
//!   account defines Y and Z" instead of a bare 404. The probe never blocks a
//!   live send and never turns an unrelated fault into a configuration error;
//! * the body is enriched per event from the structured blocks the contract
//!   carries (episode, quality, release, indexer, client, size, paths, health,
//!   versions) rather than being `summary_message` alone;
//! * the metadata links become real tap targets: the first one is Pushcut's
//!   `defaultAction`, so tapping the notification opens the title, and the rest
//!   are buttons. The choice is facet-aware — TVDb/TVMaze/Trakt for episodic
//!   libraries, TMDb/IMDb for films, AniDB/AniList/MAL/Kitsu when the title
//!   carries those ids — which Sonarr's series-only generator cannot express
//!   (`NotificationMetadataLinkGenerator.cs:9-44`);
//! * `interruptionLevel`, `sound`, `devices` and `threadId` are wired from the
//!   current API document. Sonarr sends none of them.
//!
//! # Upstream reference
//!
//! Read 2026-09-02, `https://api.pushcut.io/openapi.yaml` (the spec Pushcut's
//! own <https://www.pushcut.io/webapi> page renders) together with its
//! `definitions.yaml`, `v1/definitions.yaml`,
//! `components/schemas/notifications.yaml` and
//! `components/schemas/errors.yaml`. Facts that changed the code:
//!
//! * an invalid `API-Key` is **HTTP 401**, not the 403 Sonarr tests for, and the
//!   body is `{"error":"Invalid API-Key provided."}` (verified live against
//!   `GET /v1/devices` and `POST /v1/notifications/{name}` with a bad key,
//!   2026-09-02). Both statuses are treated as a rejected key here.
//! * `POST /notifications/{name}` answers **200** `{id}` or **202**
//!   `{id, scheduleTimestamp}`, and documents 400/401/404. The `id` is the
//!   contract's `delivery_id`; `notificationId` is marked deprecated in the
//!   spec and is not read.
//! * errors are `{error, detailCode}` where `detailCode` is one of
//!   `INTERNAL_ERROR`, `INVALID_REQUEST`, `ALREADY_EXISTS`,
//!   `SIGN_IN_WITH_APPLE`, `SIGN_IN_WITH_APPLE_TOKEN_EXPIRED`,
//!   `SIGN_IN_WITH_APPLE_AUTH_CODE_REQUIRED` (`components/schemas/errors.yaml`).
//! * the notification body carries `id`, `text`, `title`, `input`,
//!   `defaultAction`, `image`, `imageData`, `sound`, `actions`, `devices`,
//!   `threadId`, and then **either** `interruptionLevel`
//!   (`active`/`passive`/`timeSensitive`) **or** `isTimeSensitive` — they are
//!   separate `oneOf` branches, so this plugin never sends both.
//! * `delay` "Requires Server Extended subscription" and `scheduleTimestamp`
//!   schedules a future notification; neither is exposed (see the README).
//! * `x-ms-capabilities.testConnection` names `GetDevices` as the vendor's own
//!   connection probe, which is why `Test` is allowed to call it.
//!
//! Rate limits exist ("The Pushcut API applies rate limits to most requests …
//! Pro and Server Extended subscribers receive substantially higher limits",
//! <https://www.pushcut.io/support/integrations>, read 2026-09-02) but no number
//! or status is published, so a 429 is handled generically from `Retry-After`.

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, NotificationSeverity,
    PluginNotificationEpisode, PluginNotificationExternalIds, current_sdk_constraint,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

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

const PROVIDER_TYPE: &str = "pushcut";
const USER_AGENT: &str = concat!("scryer-pushcut-plugin/", env!("CARGO_PKG_VERSION"));

/// `servers[0].url` of the API document.
const API_BASE: &str = "https://api.pushcut.io/v1";
const API_HOST: &str = "api.pushcut.io";

// ---------------------------------------------------------------------------
// Size guards
//
// Pushcut publishes no length limit for `title` or `text`, so these are the
// plugin's own guards against an unbounded body rather than an API rule. They
// sit far above anything a Scryer event renders (a lock-screen notification
// shows a couple of lines), so no realistic message is affected; one that does
// hit them is trimmed with a `warnings` entry instead of being pushed whole.
// ---------------------------------------------------------------------------

const TITLE_SOFT_LIMIT: usize = 250;
const TEXT_SOFT_LIMIT: usize = 2000;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

const METADATA_LINK_AUTO: &str = "auto";
const METADATA_LINK_NONE: &str = "none";

/// Sonarr offers IMDb/TVDb/TVMaze/Trakt as a multi-select of its series-only
/// `MetadataLinkType` (`MetadataLinkType.cs:5-18`). Scryer's titles are not all
/// series, so the film and anime sites are offered too and every entry resolves
/// against the title's own facet.
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

/// Preference order for `auto`, per facet.
const AUTO_LINK_EPISODIC: &[&str] = &["tvdb", "tvmaze", "imdb", "tmdb", "anidb"];
const AUTO_LINK_OTHER: &[&str] = &["tmdb", "imdb", "tvdb"];

const INTERRUPTION_INHERIT: &str = "inherit";
const INTERRUPTION_AUTO: &str = "auto";
const INTERRUPTION_PASSIVE: &str = "passive";
const INTERRUPTION_ACTIVE: &str = "active";
const INTERRUPTION_TIME_SENSITIVE: &str = "timeSensitive";

/// `v1/definitions.yaml#/components/schemas/notification/interruptionLevel`,
/// plus the two plugin-side modes that keep the existing `time_sensitive`
/// switch meaningful.
const INTERRUPTION_LEVEL_OPTIONS: &[(&str, &str)] = &[
    (INTERRUPTION_INHERIT, "Follow the Time Sensitive setting"),
    (INTERRUPTION_AUTO, "Automatic (from event severity)"),
    (INTERRUPTION_PASSIVE, "Passive"),
    (INTERRUPTION_ACTIVE, "Active"),
    (INTERRUPTION_TIME_SENSITIVE, "Time Sensitive"),
];

const THREAD_NONE: &str = "none";
const THREAD_TITLE: &str = "title";
const THREAD_EVENT: &str = "event";

/// `threadId` — "the thread-id to be used for grouping related notifications".
const THREAD_GROUPING_OPTIONS: &[(&str, &str)] = &[
    (THREAD_NONE, "Do not group"),
    (THREAD_TITLE, "Group by title"),
    (THREAD_EVENT, "Group by event type"),
];

/// The `sound` enum from `v1/definitions.yaml`. A custom imported sound is also
/// legal ("`<your-custom-sound>`"), so an unrecognised value is never refused.
const KNOWN_SOUNDS: &[&str] = &[
    "none",
    "vibrateOnly",
    "system",
    "subtle",
    "question",
    "jobDone",
    "problem",
    "loud",
    "lasers",
];

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Pushcut".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            // Fixed by the product: every call is against api.pushcut.io.
            // Documentation rather than a prefill — there is no self-hosted
            // Pushcut.
            default_base_url: Some(API_BASE.to_string()),
            allowed_hosts: vec![API_HOST.to_string()],
            capabilities: NotificationCapabilities {
                // Pushcut renders `text` as a plain iOS notification body; the
                // API document has no markup mode.
                supports_rich_text: false,
                // `image` takes a URL, which is exactly what the contract
                // carries. No bytes are fetched or uploaded by this plugin.
                supports_images: true,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![NotificationDeliveryMode::Push],
                payload_formats: vec![NotificationPayloadFormat::PlainText],
                supported_events: general_notification_events(),
                // Every event below renders distinctly, so all three of the
                // core's per-event filters are meaningful for this channel.
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

/// A multi-value field, optionally with a fixed option set.
///
/// Scryer's notification settings UI renders a `Tag` field as a plain
/// comma-separated text input, so this is the same box the operator already
/// types into and the stored value is unchanged — which is what keeps existing
/// `metadata_links` configurations parsing.
fn tag_field(
    key: &str,
    label: &str,
    default_value: Option<&str>,
    options: &[(&str, &str)],
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        options: options
            .iter()
            .map(|(value, label)| ConfigFieldOption {
                value: (*value).to_string(),
                label: (*label).to_string(),
                config_overrides: Default::default(),
            })
            .collect(),
        ..field(
            key,
            label,
            ConfigFieldType::Tag,
            false,
            default_value,
            help_text,
        )
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "notification_name",
            "Notification Name",
            ConfigFieldType::String,
            true,
            None,
            Some(
                "The name of the notification defined in the Pushcut app. Scryer sends to this notification and never creates or edits one.",
            ),
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("A Pushcut API key, from Account -> Add API Key in the Pushcut app."),
        ),
        field(
            "time_sensitive",
            "Time Sensitive",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Marks the notification time-sensitive so it breaks through Focus. Ignored when Interruption Level is set to anything but 'Follow the Time Sensitive setting'.",
            ),
        ),
        select_field(
            "interruption_level",
            "Interruption Level",
            Some(INTERRUPTION_INHERIT),
            INTERRUPTION_LEVEL_OPTIONS,
        ),
        field(
            "include_poster",
            "Include Poster",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Sends the title's poster as the notification image. Pushcut fetches the URL itself, so it must be reachable over https or from the device's local network.",
            ),
        ),
        tag_field(
            "metadata_links",
            "Metadata Links",
            Some(METADATA_LINK_AUTO),
            METADATA_LINK_OPTIONS,
            Some(
                "Which metadata sites become tap targets. The first becomes the notification's default action, the rest become buttons. 'auto' picks the best site for the title's facet; 'none' sends no actions.",
            ),
        ),
        tag_field(
            "devices",
            "Devices",
            None,
            &[],
            Some(
                "Pushcut device names to send to, exactly as they appear in the app. Empty sends to every device on the account.",
            ),
        ),
        field(
            "sound",
            "Sound",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "One of none, vibrateOnly, system, subtle, question, jobDone, problem, loud, lasers, or the name of a sound imported into Pushcut. Empty uses the notification's own sound.",
            ),
        ),
        select_field(
            "thread_grouping",
            "Thread Grouping",
            Some(THREAD_NONE),
            THREAD_GROUPING_OPTIONS,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Everything the renderer and the sender need from configuration, resolved and
/// validated once per send so every builder below is a pure function of
/// `(request, settings)` and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    notification_name: String,
    api_key: String,
    time_sensitive: bool,
    interruption_level: String,
    include_poster: bool,
    metadata_links: Vec<String>,
    devices: Vec<String>,
    sound: Option<String>,
    thread_grouping: String,
}

impl Settings {
    /// `strict` is the Test-time posture.
    ///
    /// Rules this plugin owns — an interruption level or thread grouping outside
    /// the option set it published — are errors on every send, because the
    /// plugin would otherwise put a value on the wire that Pushcut's `oneOf`
    /// rejects. Rules that are only *probably* wrong — an unknown metadata site,
    /// a sound outside the documented enum, a `time_sensitive` switch that a
    /// chosen interruption level overrides — are refused or reported at Test
    /// time and degraded on a live send, so a channel that works today keeps
    /// working if Pushcut widens the rule.
    fn from_config(strict: bool) -> Result<(Self, Vec<String>), PluginError> {
        let mut warnings = Vec::new();

        let notification_name = required_config("notification_name").map_err(config_error)?;
        let api_key = required_config("api_key").map_err(config_error)?;

        let interruption_level = validated_choice(
            "interruption_level",
            config_value("interruption_level").as_deref(),
            INTERRUPTION_INHERIT,
            INTERRUPTION_LEVEL_OPTIONS,
        )?;
        let thread_grouping = validated_choice(
            "thread_grouping",
            config_value("thread_grouping").as_deref(),
            THREAD_NONE,
            THREAD_GROUPING_OPTIONS,
        )?;

        let time_sensitive = config_bool("time_sensitive");
        if strict && time_sensitive && interruption_level != INTERRUPTION_INHERIT {
            warnings.push(format!(
                "time_sensitive is ignored because interruption_level is set to {interruption_level}"
            ));
        }

        let configured_links = config_csv("metadata_links");
        let configured_links = if configured_links.is_empty() {
            vec![METADATA_LINK_AUTO.to_string()]
        } else {
            configured_links
        };
        let metadata_links = resolve_links(&configured_links, strict, &mut warnings)?;

        let sound = config_value("sound");
        if strict
            && let Some(sound) = sound.as_deref()
            && !KNOWN_SOUNDS.contains(&sound)
        {
            warnings.push(format!(
                "sound {sound:?} is not one of Pushcut's built-in sounds; it only works if a sound with that name is imported into the app"
            ));
        }

        Ok((
            Self {
                notification_name,
                api_key,
                time_sensitive,
                interruption_level,
                include_poster: config_bool("include_poster"),
                metadata_links,
                devices: dedup(config_csv("devices")),
                sound,
                thread_grouping,
            },
            warnings,
        ))
    }
}

/// A `Select` whose stored value has to be one this plugin published, because it
/// is used to build the request rather than passed through.
fn validated_choice(
    key: &'static str,
    raw: Option<&str>,
    default_value: &str,
    options: &[(&str, &str)],
) -> Result<String, PluginError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(default_value.to_string());
    };
    // `timeSensitive` is the camelCase wire value; accept the snake_case
    // spelling an operator is likely to type by hand.
    let normalised = raw.replace('_', "").to_ascii_lowercase();
    if let Some((value, _)) = options
        .iter()
        .find(|(value, _)| value.replace('_', "").to_ascii_lowercase() == normalised)
    {
        return Ok((*value).to_string());
    }
    Err(plugin_error(
        PluginErrorCode::InvalidConfig,
        format!("{key} is not a valid value: {raw}"),
        Some(format!("known values: {}", option_keys(options))),
    ))
}

/// The metadata sites, kept in the operator's order.
///
/// An unknown site is a typed error at Test time and a dropped entry with a
/// warning on a live send: a channel that works must not stop working because a
/// future Scryer offers a site this build does not know.
fn resolve_links(
    configured: &[String],
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, PluginError> {
    let mut valid = Vec::new();
    for entry in configured {
        let entry = entry.trim().to_ascii_lowercase();
        if entry.is_empty() {
            continue;
        }
        if !METADATA_LINK_OPTIONS.iter().any(|(key, _)| *key == entry) {
            if strict {
                return Err(plugin_error(
                    PluginErrorCode::InvalidConfig,
                    format!("metadata_links contains an unknown site: {entry}"),
                    Some(format!(
                        "known values: {}",
                        option_keys(METADATA_LINK_OPTIONS)
                    )),
                ));
            }
            warnings.push(format!(
                "metadata_links entry {entry:?} is not a site this channel knows; it was ignored"
            ));
            continue;
        }
        valid.push(entry);
    }
    Ok(dedup(valid))
}

fn option_keys(options: &[(&str, &str)]) -> String {
    options
        .iter()
        .map(|(key, _)| *key)
        .collect::<Vec<_>>()
        .join(", ")
}

fn dedup(values: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        let value = value.trim().to_string();
        if !value.is_empty() && !out.iter().any(|existing| existing == &value) {
            out.push(value);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Sonarr sends a fixed constant per event ("Episode Grabbed", "Import
/// Complete", …) and puts everything else in the body (`Pushcut.cs:32-80`).
/// Scryer's dispatcher already composes an event-specific, title-bearing heading
/// in `summary_title` ("Grabbed: Example Show"), which is strictly more
/// informative in a push notification whose title is what the lock screen shows.
fn heading(req: &PluginNotificationRequest) -> String {
    let title = req.summary_title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let app = req.app.name.trim();
    if app.is_empty() {
        "Scryer".to_string()
    } else {
        app.to_string()
    }
}

/// The notification body: the dispatcher's prose first, then the structured
/// enrichment Sonarr's one-sentence message has no room for. Every line is
/// conditional on the block actually being present, so the sparse shape the core
/// sends today renders exactly the one line the June port sent.
fn build_text(req: &PluginNotificationRequest) -> String {
    let mut lines: Vec<String> = Vec::new();

    let message = req.summary_message.trim();
    if !message.is_empty() {
        lines.push(message.to_string());
    }

    for (label, value) in detail_lines(req) {
        lines.push(format!("{label}: {value}"));
    }

    if lines.is_empty() {
        lines.push(heading(req));
    }

    lines.join("\n")
}

fn detail_lines(req: &PluginNotificationRequest) -> Vec<(&'static str, String)> {
    let mut lines: Vec<(&'static str, String)> = Vec::new();
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
        // `NotificationEventType::Download` only ever carries a FAILED download:
        // the dispatcher maps `DownloadFailed` onto it
        // (`crates/scryer-application/src/notifications/dispatcher.rs:34,418-448`,
        // release-0.19.8). A successful import is `ImportComplete`/`Upgrade`, so
        // this arm renders a failure and never an import path.
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
            push(&mut lines, "Destination", import_path(req));
        }
        NotificationEventType::ImportRejected => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "Release", release_title(req));
            push(&mut lines, "Source", source_path(req));
            push(&mut lines, "Status", import_status(req));
        }
        NotificationEventType::Rename => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "File", primary_path(req));
        }
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            push(&mut lines, "Episode", episode_display(req));
            push(&mut lines, "File", deleted_path(req));
            push(&mut lines, "Quality", quality(req));
        }
        NotificationEventType::TitleAdded | NotificationEventType::TitleDeleted => {
            push(&mut lines, "Path", title_path(req));
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
            push(&mut lines, "File", primary_path(req));
            push(&mut lines, "Languages", subtitle_languages(req));
        }
        NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestApproved
        | NotificationEventType::MediaRequestRejected
        | NotificationEventType::MediaRequestCanceled => {
            push(&mut lines, "Status", media_request_status(req));
            push(&mut lines, "Quality Profile", media_request_profile(req));
        }
        NotificationEventType::Test => {}
    }
    lines
}

fn push(lines: &mut Vec<(&'static str, String)>, label: &'static str, value: Option<String>) {
    if let Some(value) = value.map(|value| value.trim().to_string())
        && !value.is_empty()
    {
        lines.push((label, value));
    }
}

// ---------------------------------------------------------------------------
// Field values
// ---------------------------------------------------------------------------

/// The contract's rendered `episode.display` when the core filled it, otherwise
/// composed the way Sonarr composes an episode heading: `{season}x{episode}` and
/// the episode titles, or the air date for a daily episode.
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
    .or_else(|| {
        non_empty(
            req.download
                .as_ref()
                .and_then(|download| download.title.clone()),
        )
    })
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
    non_empty(
        req.release
            .as_ref()
            .and_then(|release| release.indexer.clone()),
    )
    .or_else(|| {
        non_empty(
            req.release
                .as_ref()
                .and_then(|release| release.provider.clone()),
        )
    })
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

/// The core puts the deleted path first in `file.media_updates`; `import`
/// carries an explicit list when the delete is an upgrade replacement.
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

/// Only an absolute http(s) link is offered to Pushcut: an action `url` is a tap
/// target on the device, and a relative path is a dead one.
fn manual_link(req: &PluginNotificationRequest) -> Option<String> {
    non_empty(
        req.manual_interaction
            .as_ref()
            .and_then(|interaction| interaction.link.clone()),
    )
    .filter(|link| is_absolute_http(link))
}

fn is_absolute_http(link: &str) -> bool {
    let link = link.to_ascii_lowercase();
    link.starts_with("http://") || link.starts_with("https://")
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

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn ellipsize(text: &str, budget: usize) -> String {
    if char_count(text) <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// The tap targets on the notification.
///
/// Sonarr turns each selected metadata link into an action
/// (`PushcutProxy.cs:54-61`) and stops there, so tapping the notification body
/// itself does nothing. Here the first link is also promoted to
/// `defaultAction`, which is what Pushcut runs when the notification is tapped
/// rather than long-pressed.
///
/// `ManualInteractionRequired` carries its own deep link into Scryer and wins
/// the default slot, because the point of that event is that the operator has to
/// go and do something.
fn build_actions(req: &PluginNotificationRequest, settings: &Settings) -> Vec<(String, String)> {
    let mut actions: Vec<(String, String)> = Vec::new();

    if let Some(link) = manual_link(req) {
        let app = req.app.name.trim();
        let app = if app.is_empty() { "Scryer" } else { app };
        actions.push((format!("Open in {app}"), link));
    }

    if !settings
        .metadata_links
        .iter()
        .any(|link| link == METADATA_LINK_NONE)
    {
        for choice in &settings.metadata_links {
            let resolved = if choice == METADATA_LINK_AUTO {
                auto_link(req)
            } else {
                metadata_link(req, choice)
            };
            if let Some((label, url)) = resolved
                && !actions.iter().any(|(_, existing)| existing == &url)
            {
                actions.push((label.to_string(), url));
            }
        }
    }

    actions
}

fn auto_link(req: &PluginNotificationRequest) -> Option<(&'static str, String)> {
    let order = if is_episodic(req) {
        AUTO_LINK_EPISODIC
    } else {
        AUTO_LINK_OTHER
    };
    order.iter().find_map(|key| metadata_link(req, key))
}

fn is_episodic(req: &PluginNotificationRequest) -> bool {
    req.title
        .as_ref()
        .map(|title| {
            matches!(
                title.facet.to_ascii_lowercase().as_str(),
                "series" | "anime" | "tv" | "show"
            )
        })
        .unwrap_or(false)
}

/// `NotificationMetadataLinkGenerator.GenerateLinks` on Scryer's contract.
///
/// The facet decides what "Trakt" and "TMDb" mean, which is the part Sonarr's
/// series-only model cannot express. Sonarr's `http://` URLs are emitted as
/// `https://`: every one of these sites redirects, and an http hop on a phone is
/// a needless one.
fn metadata_link(req: &PluginNotificationRequest, key: &str) -> Option<(&'static str, String)> {
    let title = req.title.as_ref()?;
    let ids = &title.external_ids;
    let episodic = is_episodic(req);

    let imdb = external_id(ids.imdb_id.as_deref(), ids, "imdb");
    let tvdb = external_id(ids.tvdb_id.as_deref(), ids, "tvdb");
    let tmdb = external_id(ids.tmdb_id.as_deref(), ids, "tmdb");
    let tvmaze = external_id(ids.tvmaze_id.as_deref(), ids, "tvmaze");

    match key {
        "imdb" => imdb.map(|id| ("IMDb", format!("https://www.imdb.com/title/{id}"))),
        "tvdb" => tvdb.map(|id| ("TVDb", format!("https://thetvdb.com/?tab=series&id={id}"))),
        "tvmaze" => tvmaze.map(|id| ("TVMaze", format!("https://www.tvmaze.com/shows/{id}"))),
        "trakt" => {
            if episodic {
                tvdb.map(|id| {
                    (
                        "Trakt",
                        format!("https://trakt.tv/search/tvdb/{id}?id_type=show"),
                    )
                })
            } else {
                tmdb.map(|id| {
                    (
                        "Trakt",
                        format!("https://trakt.tv/search/tmdb/{id}?id_type=movie"),
                    )
                })
                .or_else(|| imdb.map(|id| ("Trakt", format!("https://trakt.tv/search/imdb/{id}"))))
            }
        }
        "tmdb" => tmdb.map(|id| {
            (
                "TMDb",
                if episodic {
                    format!("https://www.themoviedb.org/tv/{id}")
                } else {
                    format!("https://www.themoviedb.org/movie/{id}")
                },
            )
        }),
        "anidb" => external_id(ids.anidb_id.as_deref(), ids, "anidb")
            .map(|id| ("AniDB", format!("https://anidb.net/anime/{id}"))),
        "anilist" => external_id(ids.anilist_ids.first().map(String::as_str), ids, "anilist")
            .map(|id| ("AniList", format!("https://anilist.co/anime/{id}"))),
        "mal" => external_id(ids.mal_ids.first().map(String::as_str), ids, "mal")
            .map(|id| ("MyAnimeList", format!("https://myanimelist.net/anime/{id}"))),
        "kitsu" => external_id(ids.kitsu_ids.first().map(String::as_str), ids, "kitsu")
            .map(|id| ("Kitsu", format!("https://kitsu.app/anime/{id}"))),
        _ => None,
    }
}

fn external_id(
    typed: Option<&str>,
    ids: &PluginNotificationExternalIds,
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
// Interruption level, thread id, image
// ---------------------------------------------------------------------------

/// The dispatcher stamps a severity on every notification (`dispatcher.rs:886`,
/// `notification_severity`), so `auto` has something real to read; the fallback
/// repeats that mapping for a request built without one.
fn severity(req: &PluginNotificationRequest) -> NotificationSeverity {
    if let Some(severity) = req.severity {
        return severity;
    }
    match req.event_type {
        NotificationEventType::Download
        | NotificationEventType::ImportRejected
        | NotificationEventType::SubtitleSearchFailed => NotificationSeverity::Error,
        NotificationEventType::HealthIssue => NotificationSeverity::Warning,
        _ => NotificationSeverity::Info,
    }
}

/// `interruptionLevel` and `isTimeSensitive` are separate `oneOf` branches of
/// the notification body (`components/schemas/notifications.yaml`), so exactly
/// one of them is ever sent.
fn interruption(req: &PluginNotificationRequest, settings: &Settings) -> (&'static str, Value) {
    match settings.interruption_level.as_str() {
        INTERRUPTION_INHERIT => ("isTimeSensitive", Value::Bool(settings.time_sensitive)),
        INTERRUPTION_AUTO => (
            "interruptionLevel",
            Value::String(
                match severity(req) {
                    NotificationSeverity::Error => INTERRUPTION_TIME_SENSITIVE,
                    NotificationSeverity::Warning => INTERRUPTION_ACTIVE,
                    NotificationSeverity::Info => INTERRUPTION_PASSIVE,
                }
                .to_string(),
            ),
        ),
        level => ("interruptionLevel", Value::String(level.to_string())),
    }
}

/// A stable `threadId` so iOS stacks a title's (or an event type's)
/// notifications instead of listing them one by one. Sonarr has no equivalent.
fn thread_id(req: &PluginNotificationRequest, settings: &Settings) -> Option<String> {
    let raw = match settings.thread_grouping.as_str() {
        THREAD_TITLE => {
            let title = req.title.as_ref()?;
            title
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .unwrap_or(title.name.as_str())
                .to_string()
        }
        THREAD_EVENT => format!("{:?}", req.event_type),
        _ => return None,
    };
    let slug: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    (!slug.is_empty()).then(|| format!("scryer-{slug}"))
}

/// Pushcut's `image`: "Name of imported image, or URL to an image. (https or
/// local network)". Pushcut fetches it, so a relative path is useless and a
/// public plain-http URL will usually not load on the device.
fn image(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Option<String> {
    if !settings.include_poster {
        return None;
    }
    let poster = poster_url(req)?;
    if !is_absolute_http(&poster) {
        warnings.push(format!(
            "the title's poster is not an absolute http(s) URL and was not attached: {poster}"
        ));
        return None;
    }
    if poster.to_ascii_lowercase().starts_with("http://") {
        warnings.push(
            "the title's poster is a plain-http URL; Pushcut loads images over https or from the device's local network".to_string(),
        );
    }
    Some(poster)
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// One `POST /v1/notifications/{name}` body, in the order the API document
/// lists the fields.
fn build_payload(req: &PluginNotificationRequest, settings: &Settings) -> (Value, Vec<String>) {
    let mut warnings = Vec::new();
    let mut payload = Map::new();

    let title = heading(req);
    if char_count(&title) > TITLE_SOFT_LIMIT {
        warnings.push(format!(
            "title trimmed to {TITLE_SOFT_LIMIT} characters before sending"
        ));
    }
    payload.insert(
        "title".to_string(),
        Value::String(ellipsize(&title, TITLE_SOFT_LIMIT)),
    );

    let text = build_text(req);
    if char_count(&text) > TEXT_SOFT_LIMIT {
        warnings.push(format!(
            "text trimmed to {TEXT_SOFT_LIMIT} characters before sending"
        ));
    }
    payload.insert(
        "text".to_string(),
        Value::String(ellipsize(&text, TEXT_SOFT_LIMIT)),
    );

    let (interruption_key, interruption_value) = interruption(req, settings);
    payload.insert(interruption_key.to_string(), interruption_value);

    if let Some(image) = image(req, settings, &mut warnings) {
        payload.insert("image".to_string(), Value::String(image));
    }

    if let Some(sound) = settings.sound.as_ref() {
        payload.insert("sound".to_string(), Value::String(sound.clone()));
    }

    let actions = build_actions(req, settings);
    if let Some((label, url)) = actions.first() {
        payload.insert(
            "defaultAction".to_string(),
            serde_json::json!({ "name": label, "url": url }),
        );
    }
    // Sonarr sends every link as an action and no default action
    // (`PushcutProxy.cs:51-61`); the list is kept whole so a long-press still
    // offers every site, including the one promoted to the default.
    payload.insert(
        "actions".to_string(),
        Value::Array(
            actions
                .iter()
                .map(|(label, url)| serde_json::json!({ "name": label, "url": url }))
                .collect(),
        ),
    );

    if !settings.devices.is_empty() {
        payload.insert(
            "devices".to_string(),
            Value::Array(
                settings
                    .devices
                    .iter()
                    .map(|device| Value::String(device.clone()))
                    .collect(),
            ),
        );
    }

    if let Some(thread_id) = thread_id(req, settings) {
        payload.insert("threadId".to_string(), Value::String(thread_id));
    }

    (Value::Object(payload), warnings)
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let (settings, mut warnings) = match Settings::from_config(req.is_test) {
        Ok(resolved) => resolved,
        Err(error) => return PluginResult::Err(error),
    };

    // Sonarr discovers a wrong notification name as a 404 with no explanation
    // (`PushcutProxy.cs:94-100` reports whatever `error` the body carried). A
    // Test can do better: the account knows which notifications and devices
    // exist, and the vendor's own `x-ms-capabilities.testConnection` points at
    // `GetDevices`.
    let mut notification_name = settings.notification_name.clone();
    if req.is_test {
        match resolve_notification_name(&settings, &mut warnings) {
            Ok(Some(resolved)) => notification_name = resolved,
            Ok(None) => {}
            Err(error) => return PluginResult::Err(error),
        }
        if !settings.devices.is_empty()
            && let Err(error) = verify_devices(&settings, &mut warnings)
        {
            return PluginResult::Err(error);
        }
    }

    let (payload, payload_warnings) = build_payload(req, &settings);
    warnings.extend(payload_warnings);

    let url = format!(
        "{API_BASE}/notifications/{}",
        path_segment(&notification_name)
    );
    let request = HttpRequest::new(url.as_str())
        .with_method("POST")
        .with_header("API-Key", settings.api_key.as_str())
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    let body = serde_json::to_vec(&payload).unwrap_or_default();

    match http::request::<Vec<u8>>(&request, Some(body)) {
        Ok(response) => classify_response(
            response.status_code(),
            response.headers(),
            &response.body(),
            &notification_name,
            warnings,
        ),
        // The host answers a refused or failed egress in-band. `api.pushcut.io`
        // is not operator-configurable, so this is the service (or Scryer's
        // egress) being unreachable rather than a setting to fix. On a
        // connection test that is worth a typed `UpstreamUnavailable`; on a live
        // send it stays on the delivery lane, like every sibling channel, so a
        // network blink is never reported as a broken channel.
        Err(error) => {
            let detail = format!("could not reach the Pushcut API at {API_HOST}: {error}");
            if req.is_test {
                return PluginResult::Err(plugin_error(
                    PluginErrorCode::UpstreamUnavailable,
                    detail,
                    Some(error.to_string()),
                ));
            }
            let mut failure = error_response(detail, Some("request_failed".to_string()));
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
    }
}

/// One authenticated `GET` against the API, used only by the Test probes.
fn get(path: &str, settings: &Settings) -> Result<(u16, Vec<u8>), String> {
    let url = format!("{API_BASE}{path}");
    let request = HttpRequest::new(url.as_str())
        .with_method("GET")
        .with_header("API-Key", settings.api_key.as_str())
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    match http::request::<Vec<u8>>(&request, None::<Vec<u8>>) {
        Ok(response) => Ok((response.status_code(), response.body())),
        Err(error) => Err(error.to_string()),
    }
}

/// `GET /v1/notifications` → the names the Pushcut app defines.
///
/// Returns the account's own spelling when it differs only by case, so the send
/// uses the client's casing rather than the operator's (common rule 5: match
/// case-insensitively, report the service's own casing).
///
/// Any failure that is not an authentication failure or a definite "no such
/// notification" degrades to a warning: a probe must never turn an unrelated
/// fault into a configuration error, and the send that follows produces the real
/// diagnosis.
fn resolve_notification_name(
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Result<Option<String>, PluginError> {
    let (status, body) = match get("/notifications", settings) {
        Ok(answer) => answer,
        Err(error) => {
            warnings.push(format!(
                "could not list the account's notifications to verify notification_name: {error}"
            ));
            return Ok(None);
        }
    };

    if matches!(status, 401 | 403) {
        return Err(rejected_key(&parse_body(&body).detail(status)));
    }
    if !(200..300).contains(&status) {
        warnings.push(format!(
            "could not list the account's notifications to verify notification_name: HTTP {status}"
        ));
        return Ok(None);
    }

    let Some(names) = id_list(&body) else {
        warnings.push(
            "the account's notification list could not be read; notification_name was not verified"
                .to_string(),
        );
        return Ok(None);
    };

    if names.iter().any(|name| name == &settings.notification_name) {
        return Ok(None);
    }
    if let Some(actual) = names
        .iter()
        .find(|name| name.eq_ignore_ascii_case(&settings.notification_name))
    {
        warnings.push(format!(
            "notification_name is spelled {actual:?} in Pushcut; that spelling was used"
        ));
        return Ok(Some(actual.clone()));
    }

    Err(plugin_error(
        PluginErrorCode::InvalidConfig,
        if names.is_empty() {
            "notification_name: the Pushcut account defines no notifications. Create one in the app first."
                .to_string()
        } else {
            format!(
                "notification_name: the Pushcut account has no notification called {:?}. It defines: {}",
                settings.notification_name,
                names.join(", ")
            )
        },
        Some(format!(
            "GET {API_BASE}/notifications returned {} name(s)",
            names.len()
        )),
    ))
}

/// `GET /v1/devices` — the operation Pushcut's own spec nominates as the
/// connection test (`x-ms-capabilities.testConnection`). Run only when the
/// channel targets named devices, because that is the setting nothing else can
/// falsify: a device name that does not exist sends the notification nowhere and
/// Pushcut still answers 200.
fn verify_devices(settings: &Settings, warnings: &mut Vec<String>) -> Result<(), PluginError> {
    let (status, body) = match get("/devices", settings) {
        Ok(answer) => answer,
        Err(error) => {
            warnings.push(format!(
                "could not list the account's devices to verify devices: {error}"
            ));
            return Ok(());
        }
    };

    if matches!(status, 401 | 403) {
        return Err(rejected_key(&parse_body(&body).detail(status)));
    }
    if !(200..300).contains(&status) {
        warnings.push(format!(
            "could not list the account's devices to verify devices: HTTP {status}"
        ));
        return Ok(());
    }

    let Some(known) = id_list(&body) else {
        warnings.push(
            "the account's device list could not be read; devices was not verified".to_string(),
        );
        return Ok(());
    };

    let unknown: Vec<&String> = settings
        .devices
        .iter()
        .filter(|device| !known.iter().any(|name| name.eq_ignore_ascii_case(device)))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }

    Err(plugin_error(
        PluginErrorCode::InvalidConfig,
        format!(
            "devices: the Pushcut account has no device called {}. Active devices: {}",
            unknown
                .iter()
                .map(|device| format!("{device:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ),
        Some(format!(
            "GET {API_BASE}/devices returned {} device(s)",
            known.len()
        )),
    ))
}

/// Both list endpoints answer an array of `{ id, … }` (`openapi.yaml`
/// `GetDevices`/`GetNotifications`). `None` means the body was not that shape,
/// which is a reason to skip the check rather than to fail it.
fn id_list(body: &[u8]) -> Option<Vec<String>> {
    let Ok(Value::Array(entries)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    Some(
        entries
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Response classification
// ---------------------------------------------------------------------------

/// `components/schemas/errors.yaml#/GeneralError` plus the success bodies of
/// `POST /notifications/{name}`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PushcutBody {
    error: Option<String>,
    detail_code: Option<String>,
    id: Option<String>,
    scheduled: bool,
    raw: Option<String>,
}

impl PushcutBody {
    fn detail(&self, status: u16) -> String {
        if let Some(error) = self.error.as_deref().map(str::trim)
            && !error.is_empty()
        {
            return match self.detail_code.as_deref() {
                Some(code) if !code.is_empty() => format!("{error} ({code})"),
                _ => error.to_string(),
            };
        }
        match self.raw.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => ellipsize(raw, 300),
            _ => format!("HTTP {status}"),
        }
    }

    fn mentions(&self, needle: &str) -> bool {
        self.error
            .as_deref()
            .map(|error| error.to_ascii_lowercase().contains(needle))
            .unwrap_or(false)
    }
}

fn parse_body(body: &[u8]) -> PushcutBody {
    let text = String::from_utf8_lossy(body).to_string();
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return PushcutBody {
            raw: Some(text),
            ..PushcutBody::default()
        };
    };
    PushcutBody {
        error: map.get("error").and_then(Value::as_str).map(str::to_string),
        detail_code: map
            .get("detailCode")
            .and_then(Value::as_str)
            .map(str::to_string),
        id: map
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        scheduled: map.contains_key("scheduleTimestamp"),
        raw: Some(text),
    }
}

/// Sonarr turns every Pushcut failure into one exception, and only inside `Test`
/// does it try to attribute it — to `API Key` on a 403, otherwise to `Url` or
/// `Host` (`PushcutProxy.cs:86-104`). Scryer's typed error lane exists on every
/// send, so the operator is always told which setting to fix, and a fault that is
/// *not* a setting stays a delivery failure the core can report as such.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    notification_name: &str,
    mut warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let answer = parse_body(body);
    let detail = answer.detail(status);

    if (200..300).contains(&status) {
        let mut response = ok_response();
        // `id` is the spec's `NotificationId`; `notificationId` is marked
        // deprecated in the same response and is deliberately not read.
        response.delivery_id = answer.id.clone();
        response.provider_status = Some(format!("http_{status}"));
        if status == 202 || answer.scheduled {
            warnings.push(
                "Pushcut accepted the notification for later delivery rather than sending it now"
                    .to_string(),
            );
        }
        response.warnings = warnings;
        return PluginResult::Ok(response);
    }

    // The account itself needs attention: Pushcut's Sign in with Apple token has
    // expired or needs re-authorisation. No Scryer setting can fix it, whatever
    // status it arrives with.
    if answer
        .detail_code
        .as_deref()
        .is_some_and(|code| code.starts_with("SIGN_IN_WITH_APPLE"))
    {
        return PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!(
                "the Pushcut account has to sign in again in the Pushcut app before it can accept notifications: {detail}"
            ),
            Some(format!("HTTP {status}: {detail}")),
        ));
    }

    match status {
        // Verified live 2026-09-02: an invalid API-Key is a 401 with
        // `{"error":"Invalid API-Key provided."}`. Sonarr only tests for 403
        // (`PushcutProxy.cs:88`), so its Test blames the wrong field today; both
        // statuses are handled here.
        401 | 403 => PluginResult::Err(rejected_key(&detail)),
        404 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "notification_name: Pushcut has no notification called {notification_name:?} ({detail}). Check the name in the Pushcut app."
            ),
            Some(format!("HTTP 404: {detail}")),
        )),
        400 | 422 => PluginResult::Err(bad_request(status, &answer, &detail)),
        // No rate limit is published, only that one exists and that it varies by
        // subscription, so the delay comes from the response rather than from a
        // constant this plugin invented.
        429 => {
            let mut failure = error_response(
                format!("Pushcut is rate limiting this account: {detail}"),
                Some("http_429".to_string()),
            );
            failure.retry_after_seconds = retry_after_seconds(headers);
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        500..=599 => {
            let mut failure = error_response(
                format!("HTTP {status}: {detail}"),
                Some(format!("http_{status}")),
            );
            failure.retry_after_seconds = retry_after_seconds(headers);
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
        _ => PluginResult::Err(plugin_error(
            PluginErrorCode::Permanent,
            format!("Pushcut rejected the request (HTTP {status}): {detail}"),
            Some(format!("HTTP {status}: {detail}")),
        )),
    }
}

fn rejected_key(detail: &str) -> PluginError {
    plugin_error(
        PluginErrorCode::AuthFailed,
        format!(
            "api_key was rejected by Pushcut: {detail}. Check the key in the Pushcut app under Account -> API Keys."
        ),
        Some(detail.to_string()),
    )
}

/// A 400 is either a setting Pushcut refused or a body this plugin built badly.
/// Only the first is something the operator can act on, so only the first is
/// `InvalidConfig`.
fn bad_request(status: u16, answer: &PushcutBody, detail: &str) -> PluginError {
    let debug = format!("HTTP {status}: {detail}");
    for (needle, setting) in [
        ("device", "devices"),
        ("sound", "sound"),
        ("image", "include_poster"),
        ("interruption", "interruption_level"),
        ("thread", "thread_grouping"),
    ] {
        if answer.mentions(needle) {
            return plugin_error(
                PluginErrorCode::InvalidConfig,
                format!("{setting} was rejected by Pushcut: {detail}"),
                Some(debug),
            );
        }
    }
    plugin_error(
        PluginErrorCode::Permanent,
        format!("Pushcut rejected the notification this plugin built: {detail}"),
        Some(debug),
    )
}

fn retry_after_seconds(headers: &BTreeMap<String, String>) -> Option<i64> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<i64>().ok())
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

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// One arm per operation this plugin exports. `action` is not one of them: the descriptor advertises no action, so the host does not route
/// one here and the arm answers **in-band** with `Unsupported` rather than
/// trapping. A trap under a component costs the whole instance and replaces the
/// plugin's own diagnosis with a generic ABI failure.
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
        NotificationMediaUpdateType, PluginNotificationApp, PluginNotificationApplicationUpdate,
        PluginNotificationDownload, PluginNotificationFile, PluginNotificationHealth,
        PluginNotificationImport, PluginNotificationManualInteraction, PluginNotificationMediaFile,
        PluginNotificationMediaUpdate, PluginNotificationRelease, PluginNotificationTitle,
    };

    fn settings() -> Settings {
        Settings {
            notification_name: "Scryer".to_string(),
            api_key: "pushcutkey".to_string(),
            time_sensitive: false,
            interruption_level: INTERRUPTION_INHERIT.to_string(),
            include_poster: false,
            metadata_links: vec![METADATA_LINK_AUTO.to_string()],
            devices: Vec::new(),
            sound: None,
            thread_grouping: THREAD_NONE.to_string(),
        }
    }

    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: None,
            occurred_at: Some("2026-09-02T12:00:00+00:00".to_string()),
            correlation_id: None,
            actor: None,
            severity: Some(NotificationSeverity::Info),
            is_test: matches!(event_type, NotificationEventType::Test),
            summary_title: "Grabbed: Example Show".to_string(),
            summary_message: "Grabbed 'Example.Show.S01E01' for 'Example Show'.".to_string(),
            app: PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "0.19.8".to_string(),
            },
            title: None,
            episode: None,
            episodes: Vec::new(),
            release: None,
            download: None,
            import: None,
            health: None,
            file: None,
            media_files: Vec::new(),
            application_update: None,
            manual_interaction: None,
            media_request: None,
        }
    }

    fn series_title() -> PluginNotificationTitle {
        PluginNotificationTitle {
            id: Some("title-1".to_string()),
            name: "Example Show".to_string(),
            facet: "series".to_string(),
            year: Some(2024),
            slug: None,
            path: Some("/media/TV/Example Show".to_string()),
            overview: None,
            sort_title: None,
            background_url: None,
            poster_url: Some("https://images.test/poster.jpg".to_string()),
            tags: Vec::new(),
            aliases: Vec::new(),
            original_language: None,
            original_country: None,
            external_ids: PluginNotificationExternalIds {
                tvdb_id: Some("12345".to_string()),
                imdb_id: Some("tt0903747".to_string()),
                tvmaze_id: Some("82".to_string()),
                ..PluginNotificationExternalIds::default()
            },
        }
    }

    fn movie_title() -> PluginNotificationTitle {
        PluginNotificationTitle {
            facet: "movie".to_string(),
            name: "Example Movie".to_string(),
            external_ids: PluginNotificationExternalIds {
                tmdb_id: Some("603".to_string()),
                imdb_id: Some("tt0133093".to_string()),
                ..PluginNotificationExternalIds::default()
            },
            ..series_title()
        }
    }

    fn payload_of(req: &PluginNotificationRequest, settings: &Settings) -> Value {
        build_payload(req, settings).0
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn err_of(result: PluginResult<PluginNotificationResponse>) -> PluginError {
        match result {
            PluginResult::Err(error) => error,
            PluginResult::Ok(response) => panic!("expected a typed plugin error, got {response:?}"),
        }
    }

    fn ok_of(result: PluginResult<PluginNotificationResponse>) -> PluginNotificationResponse {
        match result {
            PluginResult::Ok(response) => response,
            PluginResult::Err(error) => panic!("expected a delivery result, got {error:?}"),
        }
    }

    fn action(label: &str, url: &str) -> (String, String) {
        (label.to_string(), url.to_string())
    }

    // -----------------------------------------------------------------------
    // Descriptor and configuration
    // -----------------------------------------------------------------------

    #[test]
    fn descriptor_declares_the_channel_the_host_needs_to_route() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = &descriptor.provider else {
            panic!("pushcut must describe a notification provider");
        };
        assert_eq!(notification.provider_type, PROVIDER_TYPE);
        assert_eq!(notification.allowed_hosts, vec![API_HOST.to_string()]);
        assert_eq!(notification.default_base_url.as_deref(), Some(API_BASE));
        assert!(notification.capabilities.supports_test);
        assert!(
            notification.capabilities.supports_images,
            "Pushcut's image field takes the poster URL the contract carries"
        );
        assert!(
            !notification.capabilities.supports_rich_text,
            "the API document describes no markup mode"
        );
        assert!(!notification.capabilities.requires_host_filesystem);
        assert!(!notification.capabilities.requires_host_process);
        assert_eq!(
            notification.capabilities.payload_formats,
            vec![NotificationPayloadFormat::PlainText]
        );
        assert!(
            notification
                .capabilities
                .event_options
                .supports_upgrade_filter
        );
    }

    #[test]
    fn config_keys_are_the_public_contract_the_june_port_published() {
        let fields = config_fields();
        let by_key = |key: &str| {
            fields
                .iter()
                .find(|field| field.key == key)
                .unwrap_or_else(|| panic!("{key} must stay a config field"))
                .clone()
        };

        // Sonarr's four settings, unchanged in key and type.
        assert!(by_key("notification_name").required);
        assert_eq!(by_key("api_key").field_type, ConfigFieldType::Password);
        assert_eq!(by_key("time_sensitive").field_type, ConfigFieldType::Bool);
        assert_eq!(by_key("include_poster").field_type, ConfigFieldType::Bool);

        // M1: the CSV string becomes a Tag field carrying the option set Sonarr
        // renders as a multi-select, plus the facets Sonarr has no model for.
        let links = by_key("metadata_links");
        assert_eq!(links.field_type, ConfigFieldType::Tag);
        assert_eq!(links.default_value.as_deref(), Some(METADATA_LINK_AUTO));
        for expected in ["imdb", "tvdb", "tvmaze", "trakt", "tmdb", "anidb"] {
            assert!(
                links.options.iter().any(|option| option.value == expected),
                "metadata_links must offer {expected}"
            );
        }

        // New, from the current API document.
        assert_eq!(
            by_key("interruption_level").default_value.as_deref(),
            Some(INTERRUPTION_INHERIT),
            "an existing channel must keep using the time_sensitive switch"
        );
        assert_eq!(by_key("devices").field_type, ConfigFieldType::Tag);
        assert_eq!(
            by_key("thread_grouping").default_value.as_deref(),
            Some(THREAD_NONE),
            "grouping is opt-in so an existing channel looks unchanged"
        );
    }

    #[test]
    fn an_unknown_select_value_is_a_typed_config_error_and_snake_case_is_accepted() {
        assert_eq!(
            validated_choice(
                "interruption_level",
                Some("time_sensitive"),
                INTERRUPTION_INHERIT,
                INTERRUPTION_LEVEL_OPTIONS
            )
            .expect("the snake_case spelling of the wire value"),
            INTERRUPTION_TIME_SENSITIVE
        );
        assert_eq!(
            validated_choice(
                "interruption_level",
                None,
                INTERRUPTION_INHERIT,
                INTERRUPTION_LEVEL_OPTIONS
            )
            .expect("an unset select falls back to its default"),
            INTERRUPTION_INHERIT
        );
        let error = validated_choice(
            "interruption_level",
            Some("shouty"),
            INTERRUPTION_INHERIT,
            INTERRUPTION_LEVEL_OPTIONS,
        )
        .expect_err("an unknown level cannot go on the wire");
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("interruption_level"));
    }

    #[test]
    fn an_unknown_metadata_site_is_strict_at_test_time_and_degrades_on_a_send() {
        let configured = vec!["imdb".to_string(), "myspace".to_string()];

        let mut warnings = Vec::new();
        let resolved =
            resolve_links(&configured, false, &mut warnings).expect("a live send keeps working");
        assert_eq!(resolved, vec!["imdb".to_string()]);
        assert_eq!(
            warnings.len(),
            1,
            "the operator is still told: {warnings:?}"
        );

        let strict = resolve_links(&configured, true, &mut Vec::new())
            .expect_err("Test refuses what it cannot render");
        assert_eq!(strict.code, PluginErrorCode::InvalidConfig);
        assert!(strict.public_message.contains("myspace"));
    }

    #[test]
    fn metadata_sites_keep_the_operators_order_and_are_deduplicated() {
        let configured = vec![
            "TVDb".to_string(),
            "imdb".to_string(),
            "tvdb".to_string(),
            "  ".to_string(),
        ];
        assert_eq!(
            resolve_links(&configured, true, &mut Vec::new()).expect("all sites are known"),
            vec!["tvdb".to_string(), "imdb".to_string()]
        );
    }

    // -----------------------------------------------------------------------
    // Payload
    // -----------------------------------------------------------------------

    #[test]
    fn the_sparse_request_the_core_sends_today_renders_sonarrs_payload() {
        let payload = payload_of(&request(NotificationEventType::Grab), &settings());
        assert_eq!(payload["title"], "Grabbed: Example Show");
        assert_eq!(
            payload["text"],
            "Grabbed 'Example.Show.S01E01' for 'Example Show'."
        );
        assert_eq!(payload["isTimeSensitive"], false);
        assert_eq!(payload["actions"], serde_json::json!([]));
        assert!(payload.get("image").is_none(), "include_poster is off");
        assert!(payload.get("devices").is_none());
        assert!(payload.get("threadId").is_none());
        assert!(
            payload.get("interruptionLevel").is_none(),
            "isTimeSensitive and interruptionLevel are separate oneOf branches"
        );
    }

    #[test]
    fn a_grab_renders_the_structured_blocks_sonarrs_one_sentence_cannot() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        req.episode = Some(PluginNotificationEpisode {
            display: Some("1x01 - Pilot".to_string()),
            ..PluginNotificationEpisode::default()
        });
        req.release = Some(PluginNotificationRelease {
            source_title: Some("Example.Show.S01E01.1080p.WEB".to_string()),
            quality: Some("WEBDL-1080p".to_string()),
            release_group: Some("SCRYER".to_string()),
            indexer: Some("Example Indexer".to_string()),
            ..PluginNotificationRelease::default()
        });
        req.download = Some(PluginNotificationDownload {
            client_name: Some("qBittorrent".to_string()),
            size_bytes: Some(2_147_483_648),
            ..PluginNotificationDownload::default()
        });

        let payload = payload_of(&req, &settings());
        let text = payload["text"].as_str().expect("text is a string");
        assert!(text.starts_with("Grabbed 'Example.Show.S01E01'"));
        assert!(text.contains("Episode: 1x01 - Pilot"));
        assert!(text.contains("Quality: WEBDL-1080p"));
        assert!(text.contains("Release: Example.Show.S01E01.1080p.WEB"));
        assert!(text.contains("Release Group: SCRYER"));
        assert!(text.contains("Indexer: Example Indexer"));
        assert!(text.contains("Size: 2 GB"));
        assert!(text.contains("Client: qBittorrent"));
    }

    #[test]
    fn a_download_event_renders_a_failure_because_that_is_all_it_ever_carries() {
        let mut req = request(NotificationEventType::Download);
        req.summary_message = "Download failed: Example.Show.S01E01".to_string();
        req.severity = Some(NotificationSeverity::Error);
        req.download = Some(PluginNotificationDownload {
            client_name: Some("SABnzbd".to_string()),
            status: Some("failed".to_string()),
            status_message: Some("unpack error".to_string()),
            ..PluginNotificationDownload::default()
        });
        let payload = payload_of(&req, &settings());
        let text = payload["text"].as_str().expect("text is a string");
        assert!(text.contains("Download failed"));
        assert!(text.contains("Status: unpack error"));
        assert!(
            !text.contains("Destination"),
            "an import path would be a lie on a failed download"
        );
    }

    #[test]
    fn every_event_type_renders_rather_than_failing() {
        for event_type in general_notification_events() {
            let mut req = request(event_type);
            req.title = Some(series_title());
            req.file = Some(PluginNotificationFile {
                primary_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
                media_updates: vec![PluginNotificationMediaUpdate {
                    path: "/media/TV/Example Show/S01E01.mkv".to_string(),
                    update_type: NotificationMediaUpdateType::Deleted,
                }],
            });
            req.import = Some(PluginNotificationImport {
                dest_path: Some("/media/TV/Example Show/S01E01.mkv".to_string()),
                ..PluginNotificationImport::default()
            });
            req.health = Some(PluginNotificationHealth {
                code: Some("IndexerStatusCheck".to_string()),
                message: Some("Indexers unavailable".to_string()),
                ..PluginNotificationHealth::default()
            });
            req.application_update = Some(PluginNotificationApplicationUpdate {
                current_version: Some("0.19.7".to_string()),
                target_version: Some("0.19.8".to_string()),
                ..PluginNotificationApplicationUpdate::default()
            });
            req.manual_interaction = Some(PluginNotificationManualInteraction {
                reason: Some("needs a decision".to_string()),
                ..PluginNotificationManualInteraction::default()
            });
            req.media_files = vec![PluginNotificationMediaFile {
                path: "/media/TV/Example Show/S01E01.mkv".to_string(),
                subtitle_languages: vec!["English".to_string()],
                ..PluginNotificationMediaFile::default()
            }];

            let payload = payload_of(&req, &settings());
            assert!(
                !payload["title"].as_str().unwrap_or_default().is_empty(),
                "{event_type:?} must render a title"
            );
            assert!(
                !payload["text"].as_str().unwrap_or_default().is_empty(),
                "{event_type:?} must render a body"
            );
        }
    }

    #[test]
    fn an_empty_request_still_renders_a_body() {
        let mut req = request(NotificationEventType::Test);
        req.summary_title = String::new();
        req.summary_message = String::new();
        let payload = payload_of(&req, &settings());
        assert_eq!(payload["title"], "Scryer");
        assert_eq!(payload["text"], "Scryer");
    }

    #[test]
    fn an_over_long_body_is_trimmed_with_a_warning_rather_than_sent_whole() {
        let mut req = request(NotificationEventType::Grab);
        req.summary_title = "T".repeat(TITLE_SOFT_LIMIT + 40);
        req.summary_message = "M".repeat(TEXT_SOFT_LIMIT + 400);
        let (payload, warnings) = build_payload(&req, &settings());
        assert_eq!(
            char_count(payload["title"].as_str().expect("title")),
            TITLE_SOFT_LIMIT
        );
        assert_eq!(
            char_count(payload["text"].as_str().expect("text")),
            TEXT_SOFT_LIMIT
        );
        assert!(payload["text"].as_str().expect("text").ends_with('…'));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    #[test]
    fn metadata_links_become_actions_and_the_first_one_is_the_tap_target() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());
        let mut settings = settings();
        settings.metadata_links = vec!["imdb".to_string(), "tvdb".to_string()];

        let payload = payload_of(&req, &settings);
        assert_eq!(
            payload["defaultAction"],
            serde_json::json!({ "name": "IMDb", "url": "https://www.imdb.com/title/tt0903747" })
        );
        assert_eq!(
            payload["actions"],
            serde_json::json!([
                { "name": "IMDb", "url": "https://www.imdb.com/title/tt0903747" },
                { "name": "TVDb", "url": "https://thetvdb.com/?tab=series&id=12345" },
            ])
        );
    }

    #[test]
    fn auto_picks_the_best_site_for_the_titles_facet() {
        let mut series = request(NotificationEventType::Grab);
        series.title = Some(series_title());
        assert_eq!(
            build_actions(&series, &settings()),
            vec![action("TVDb", "https://thetvdb.com/?tab=series&id=12345")],
            "an episodic title leads with TVDb"
        );

        let mut movie = request(NotificationEventType::Grab);
        movie.title = Some(movie_title());
        assert_eq!(
            build_actions(&movie, &settings()),
            vec![action("TMDb", "https://www.themoviedb.org/movie/603")],
            "a film leads with TMDb, which Sonarr's series-only generator cannot do"
        );
    }

    #[test]
    fn trakt_and_tmdb_follow_the_facet_and_anime_ids_are_offered() {
        let mut movie = request(NotificationEventType::Grab);
        movie.title = Some(movie_title());
        assert_eq!(
            metadata_link(&movie, "trakt"),
            Some((
                "Trakt",
                "https://trakt.tv/search/tmdb/603?id_type=movie".to_string()
            ))
        );

        let mut anime = request(NotificationEventType::Grab);
        let mut title = series_title();
        title.facet = "anime".to_string();
        title.external_ids.anidb_id = Some("979".to_string());
        title.external_ids.anilist_ids = vec!["1535".to_string()];
        anime.title = Some(title);
        assert_eq!(
            metadata_link(&anime, "anidb"),
            Some(("AniDB", "https://anidb.net/anime/979".to_string()))
        );
        assert_eq!(
            metadata_link(&anime, "anilist"),
            Some(("AniList", "https://anilist.co/anime/1535".to_string()))
        );
        assert_eq!(
            metadata_link(&anime, "trakt"),
            Some((
                "Trakt",
                "https://trakt.tv/search/tvdb/12345?id_type=show".to_string()
            )),
            "an episodic facet still resolves Trakt through TVDb, as Sonarr does"
        );
    }

    #[test]
    fn an_id_only_present_in_by_source_still_produces_a_link() {
        let mut req = request(NotificationEventType::Grab);
        let mut title = series_title();
        title.external_ids = PluginNotificationExternalIds::default();
        title
            .external_ids
            .by_source
            .insert("tvdb".to_string(), vec!["777".to_string()]);
        req.title = Some(title);
        assert_eq!(
            metadata_link(&req, "tvdb"),
            Some(("TVDb", "https://thetvdb.com/?tab=series&id=777".to_string()))
        );
    }

    #[test]
    fn none_suppresses_every_action_and_a_missing_id_is_simply_skipped() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        let mut off = settings();
        off.metadata_links = vec![METADATA_LINK_NONE.to_string()];
        assert!(build_actions(&req, &off).is_empty());

        let mut tmdb = settings();
        tmdb.metadata_links = vec!["tmdb".to_string()];
        assert!(
            build_actions(&req, &tmdb).is_empty(),
            "the series fixture carries no TMDb id, so there is nothing to link"
        );

        let mut without_title = request(NotificationEventType::Test);
        without_title.title = None;
        assert!(build_actions(&without_title, &settings()).is_empty());
    }

    #[test]
    fn a_manual_interaction_link_wins_the_default_action() {
        let mut req = request(NotificationEventType::ManualInteractionRequired);
        req.title = Some(series_title());
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            reason: Some("needs a decision".to_string()),
            link: Some("https://scryer.example/queue/1".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        let payload = payload_of(&req, &settings());
        assert_eq!(payload["defaultAction"]["name"], "Open in Scryer");
        assert_eq!(
            payload["defaultAction"]["url"],
            "https://scryer.example/queue/1"
        );

        // A relative link is a dead tap target, so the metadata link takes over.
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            link: Some("/queue/1".to_string()),
            ..PluginNotificationManualInteraction::default()
        });
        assert_eq!(
            payload_of(&req, &settings())["defaultAction"]["name"],
            "TVDb"
        );
    }

    // -----------------------------------------------------------------------
    // Interruption level, thread id, image
    // -----------------------------------------------------------------------

    #[test]
    fn interruption_level_and_time_sensitive_are_never_sent_together() {
        let mut req = request(NotificationEventType::Grab);
        req.severity = Some(NotificationSeverity::Error);

        let mut legacy = settings();
        legacy.time_sensitive = true;
        let payload = payload_of(&req, &legacy);
        assert_eq!(payload["isTimeSensitive"], true);
        assert!(payload.get("interruptionLevel").is_none());

        let mut explicit = settings();
        explicit.time_sensitive = true;
        explicit.interruption_level = INTERRUPTION_PASSIVE.to_string();
        let payload = payload_of(&req, &explicit);
        assert_eq!(payload["interruptionLevel"], INTERRUPTION_PASSIVE);
        assert!(
            payload.get("isTimeSensitive").is_none(),
            "the two live in different oneOf branches of the request body"
        );
    }

    #[test]
    fn automatic_interruption_follows_the_dispatchers_severity() {
        let mut auto = settings();
        auto.interruption_level = INTERRUPTION_AUTO.to_string();

        let mut failure = request(NotificationEventType::Download);
        failure.severity = Some(NotificationSeverity::Error);
        assert_eq!(
            payload_of(&failure, &auto)["interruptionLevel"],
            INTERRUPTION_TIME_SENSITIVE
        );

        let mut health = request(NotificationEventType::HealthIssue);
        health.severity = Some(NotificationSeverity::Warning);
        assert_eq!(
            payload_of(&health, &auto)["interruptionLevel"],
            INTERRUPTION_ACTIVE
        );

        let grab = request(NotificationEventType::Grab);
        assert_eq!(
            payload_of(&grab, &auto)["interruptionLevel"],
            INTERRUPTION_PASSIVE
        );
    }

    #[test]
    fn a_missing_severity_falls_back_to_the_dispatchers_own_mapping() {
        let mut failure = request(NotificationEventType::Download);
        failure.severity = None;
        assert_eq!(severity(&failure), NotificationSeverity::Error);

        let mut health = request(NotificationEventType::HealthIssue);
        health.severity = None;
        assert_eq!(severity(&health), NotificationSeverity::Warning);

        let mut rename = request(NotificationEventType::Rename);
        rename.severity = None;
        assert_eq!(severity(&rename), NotificationSeverity::Info);
    }

    #[test]
    fn thread_grouping_is_opt_in_and_stable() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        assert!(payload_of(&req, &settings()).get("threadId").is_none());

        let mut by_title = settings();
        by_title.thread_grouping = THREAD_TITLE.to_string();
        assert_eq!(payload_of(&req, &by_title)["threadId"], "scryer-title-1");

        let mut by_event = settings();
        by_event.thread_grouping = THREAD_EVENT.to_string();
        assert_eq!(payload_of(&req, &by_event)["threadId"], "scryer-grab");

        // A title with no id falls back to its name, sanitised.
        let mut unnamed = request(NotificationEventType::Grab);
        let mut title = series_title();
        title.id = None;
        unnamed.title = Some(title);
        assert_eq!(
            payload_of(&unnamed, &by_title)["threadId"],
            "scryer-example-show"
        );
    }

    #[test]
    fn the_poster_becomes_pushcuts_image_only_when_asked_for() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(series_title());

        assert!(payload_of(&req, &settings()).get("image").is_none());

        let mut with_poster = settings();
        with_poster.include_poster = true;
        assert_eq!(
            payload_of(&req, &with_poster)["image"],
            "https://images.test/poster.jpg"
        );

        // Pushcut fetches the URL itself, so a relative one is dropped.
        let mut relative = series_title();
        relative.poster_url = Some("/images/poster.jpg".to_string());
        req.title = Some(relative);
        let (payload, warnings) = build_payload(&req, &with_poster);
        assert!(payload.get("image").is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        // Plain http is sent but flagged: it only loads on the local network.
        let mut insecure = series_title();
        insecure.poster_url = Some("http://nas.local/poster.jpg".to_string());
        req.title = Some(insecure);
        let (payload, warnings) = build_payload(&req, &with_poster);
        assert_eq!(payload["image"], "http://nas.local/poster.jpg");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn devices_and_sound_are_sent_only_when_configured() {
        let req = request(NotificationEventType::Grab);
        let payload = payload_of(&req, &settings());
        assert!(payload.get("devices").is_none());
        assert!(payload.get("sound").is_none());

        let mut targeted = settings();
        targeted.devices = vec!["Simon's iPhone".to_string(), "iPad".to_string()];
        targeted.sound = Some("jobDone".to_string());
        let payload = payload_of(&req, &targeted);
        assert_eq!(
            payload["devices"],
            serde_json::json!(["Simon's iPhone", "iPad"])
        );
        assert_eq!(payload["sound"], "jobDone");
    }

    // -----------------------------------------------------------------------
    // Response classification
    // -----------------------------------------------------------------------

    #[test]
    fn a_sent_notification_reports_the_providers_id_as_the_delivery_id() {
        let response = ok_of(classify_response(
            200,
            &BTreeMap::new(),
            br#"{"id":"1snurqjF5vLxoUq09Q358","notificationId":"legacy"}"#,
            "Scryer",
            Vec::new(),
        ));
        assert!(response.success);
        assert_eq!(
            response.delivery_id.as_deref(),
            Some("1snurqjF5vLxoUq09Q358")
        );
        assert_eq!(response.provider_status.as_deref(), Some("http_200"));
        assert!(response.warnings.is_empty());
    }

    #[test]
    fn a_scheduled_acceptance_is_a_success_the_operator_is_told_about() {
        let response = ok_of(classify_response(
            202,
            &BTreeMap::new(),
            br#"{"id":"abc","scheduleTimestamp":1788338915000}"#,
            "Scryer",
            Vec::new(),
        ));
        assert!(response.success);
        assert_eq!(response.delivery_id.as_deref(), Some("abc"));
        assert_eq!(response.warnings.len(), 1, "{:?}", response.warnings);
    }

    /// Verified against the live API on 2026-09-02: an invalid key is a **401**
    /// with `{"error":"Invalid API-Key provided."}`. Sonarr only tests for 403
    /// (`PushcutProxy.cs:88`), so its Test misattributes this today.
    #[test]
    fn a_rejected_key_is_authfailed_on_401_and_on_403() {
        for status in [401u16, 403] {
            let error = err_of(classify_response(
                status,
                &BTreeMap::new(),
                br#"{"error":"Invalid API-Key provided."}"#,
                "Scryer",
                Vec::new(),
            ));
            assert_eq!(error.code, PluginErrorCode::AuthFailed, "status {status}");
            assert!(
                error.public_message.contains("api_key"),
                "the operator has to be told which setting: {error:?}"
            );
            assert!(error.public_message.contains("Invalid API-Key provided."));
        }
    }

    #[test]
    fn an_undefined_notification_is_invalidconfig_naming_the_setting() {
        let error = err_of(classify_response(
            404,
            &BTreeMap::new(),
            br#"{"error":"Notification not found."}"#,
            "Missing One",
            Vec::new(),
        ));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("notification_name"));
        assert!(error.public_message.contains("Missing One"));
    }

    #[test]
    fn a_bad_request_is_attributed_to_a_setting_only_when_it_names_one() {
        let device = err_of(classify_response(
            400,
            &BTreeMap::new(),
            br#"{"error":"Unknown device: Kitchen","detailCode":"INVALID_REQUEST"}"#,
            "Scryer",
            Vec::new(),
        ));
        assert_eq!(device.code, PluginErrorCode::InvalidConfig);
        assert!(device.public_message.contains("devices"));

        let payload = err_of(classify_response(
            400,
            &BTreeMap::new(),
            br#"{"error":"Request body is malformed","detailCode":"INVALID_REQUEST"}"#,
            "Scryer",
            Vec::new(),
        ));
        assert_eq!(
            payload.code,
            PluginErrorCode::Permanent,
            "a body only this plugin builds is a plugin bug, not a setting"
        );
        assert!(
            payload
                .debug_message
                .expect("the raw detail is kept")
                .contains("INVALID_REQUEST")
        );
    }

    #[test]
    fn a_sign_in_with_apple_detail_code_is_an_account_authfailure_whatever_the_status() {
        let error = err_of(classify_response(
            400,
            &BTreeMap::new(),
            br#"{"error":"Token expired","detailCode":"SIGN_IN_WITH_APPLE_TOKEN_EXPIRED"}"#,
            "Scryer",
            Vec::new(),
        ));
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("sign in again"));
    }

    #[test]
    fn a_rate_limit_is_a_delivery_failure_carrying_the_providers_own_delay() {
        let response = ok_of(classify_response(
            429,
            &headers(&[("Retry-After", "90")]),
            br#"{"error":"Too many requests"}"#,
            "Scryer",
            Vec::new(),
        ));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_429"));
        assert_eq!(response.retry_after_seconds, Some(90));
        assert!(
            response
                .error
                .expect("the provider's own text")
                .contains("Too many requests")
        );
    }

    #[test]
    fn a_server_fault_is_a_delivery_failure_not_a_configuration_error() {
        let response = ok_of(classify_response(
            503,
            &BTreeMap::new(),
            b"upstream exploded",
            "Scryer",
            vec!["carried".to_string()],
        ));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_503"));
        assert_eq!(
            response.warnings,
            vec!["carried".to_string()],
            "warnings raised while building the payload survive a failure"
        );
        assert!(
            response
                .error
                .expect("a non-JSON body still yields a message")
                .contains("upstream exploded")
        );
    }

    // -----------------------------------------------------------------------
    // Test-time probes
    // -----------------------------------------------------------------------

    #[test]
    fn the_notification_list_is_read_as_the_spec_documents_it() {
        assert_eq!(
            id_list(br#"[{"id":"Scryer","title":"t"},{"id":" Other "}]"#),
            Some(vec!["Scryer".to_string(), "Other".to_string()])
        );
        assert_eq!(id_list(b"[]"), Some(Vec::new()));
        assert_eq!(
            id_list(b"{}"),
            None,
            "a body that is not the documented array must not be read as 'no notifications'"
        );
        assert_eq!(id_list(b"not json"), None);
    }

    #[test]
    fn retry_after_is_read_case_insensitively_and_floored() {
        assert_eq!(
            retry_after_seconds(&headers(&[("retry-after", " 30 ")])),
            Some(30)
        );
        assert_eq!(
            retry_after_seconds(&headers(&[("Retry-After", "0")])),
            Some(1)
        );
        assert_eq!(
            retry_after_seconds(&headers(&[(
                "Retry-After",
                "Wed, 02 Sep 2026 08:00:00 GMT"
            )])),
            None,
            "an HTTP-date delay is not seconds and is not guessed at"
        );
        assert_eq!(retry_after_seconds(&BTreeMap::new()), None);
    }

    #[test]
    fn sizes_render_the_way_sonarr_renders_them() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(2_147_483_648), "2 GB");
    }

    #[test]
    fn an_episode_display_is_composed_when_the_core_did_not_render_one() {
        let mut req = request(NotificationEventType::Grab);
        req.episodes = vec![
            PluginNotificationEpisode {
                season_number: Some("1".to_string()),
                episode_number: Some("1".to_string()),
                title: Some("Pilot".to_string()),
                ..PluginNotificationEpisode::default()
            },
            PluginNotificationEpisode {
                season_number: Some("1".to_string()),
                episode_number: Some("2".to_string()),
                title: Some("Second".to_string()),
                ..PluginNotificationEpisode::default()
            },
        ];
        assert_eq!(
            episode_display(&req).as_deref(),
            Some("1x01x02 - Pilot + Second")
        );

        // A daily episode has no number, so Sonarr shows the air date.
        let mut daily = request(NotificationEventType::Grab);
        daily.episode = Some(PluginNotificationEpisode {
            air_date: Some("2026-09-02".to_string()),
            title: Some("Tonight".to_string()),
            ..PluginNotificationEpisode::default()
        });
        assert_eq!(
            episode_display(&daily).as_deref(),
            Some("2026-09-02 - Tonight")
        );
    }
}
