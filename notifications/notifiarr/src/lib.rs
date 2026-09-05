//! Notifiarr notifications, as a WASI Preview 2 component.
//!
//! # Why this channel is different from every other one in the family
//!
//! Every other notification channel posts to an endpoint the operator owns, so
//! the payload shape is this plugin's business. Notifiarr's endpoints are
//! *server-side integrations*: `https://notifiarr.com/api/v1/notification/sonarr`
//! is parsed by Notifiarr as Sonarr's webhook schema (`eventType`, `series`,
//! `episodes`, `release`, `episodeFile`, `downloadClient`, …) and rendered into a
//! Discord card by Notifiarr's own Sonarr integration. The shape is a contract
//! with a third party, not a rendering choice.
//!
//! The June port posted Scryer's *own* `PluginNotificationRequest` JSON
//! (`to_webhook_json`, snake_case, `event_type`/`summary_title`/`title`) to that
//! Sonarr endpoint. Notifiarr answers a foreign body with `400`, and the port
//! then rewrote that `400` into `success: true` with a warning — so every
//! delivery this channel ever made failed silently. Both halves of that are
//! fixed here.
//!
//! # The two integrations
//!
//! Notifiarr publishes a generic integration alongside the per-app ones:
//! **Passthrough** (`/api/v1/notification/passthrough/{apikey}`,
//! <https://notifiarr.wiki/pages/integrations/passthrough/>) accepts an
//! app-defined `{notification, discord}` document and renders it directly. It
//! carries every event Scryer has, it does not pretend a movie is a series, and
//! it is what Shoutrrr and Apprise use. It is this plugin's default.
//!
//! **Sonarr** (`/api/v1/notification/sonarr`) is kept as a second mode, because
//! it is the only way to reach Notifiarr's *Sonarr* integration — its per-trigger
//! channel picker and its media cards — and reaching it is the Sonarr-parity
//! floor for this channel. In that mode the plugin builds a genuine Sonarr
//! `WebhookPayload` (camelCase members, `NullValueHandling.Ignore`, PascalCase
//! `eventType`, camelCase enum values —
//! `NzbDrone.Common/Serializer/Newtonsoft.Json/Json.cs`) from Scryer's contract.
//! Its limits are documented in the README and reported as `warnings`: it is a
//! TV schema, so a movie facet is rendered as a series, and the Scryer-only
//! events (subtitles, media requests, post-processing) have no member of
//! `WebhookEventType` to occupy and are refused with a typed `Unsupported`
//! naming the passthrough integration instead.
//!
//! # Why the delivery path is local rather than `notify_common::send_json`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")` and treats every 2xx as a success. Notifiarr needs both
//! halves of that undone: it answers `200` with `{"result":"error", …}` when the
//! integration rejects the payload, it distinguishes an invalid API key (401)
//! from an integration that is off or unassigned (400), it is fronted by
//! Cloudflare (520-524, and a `400 text/html` that is Cloudflare rather than
//! Notifiarr — `Notifiarr/notifiarr:pkg/website/website.go:53-60`), and it
//! rate-limits per account tier with a `Retry-After` the core can act on.

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, NotificationSeverity,
    PluginNotificationEpisode, PluginNotificationRelease, PluginNotificationTargetResult,
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

const PROVIDER_TYPE: &str = "notifiarr";
const USER_AGENT: &str = concat!("scryer-notifiarr-plugin/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// Notifiarr's API
//
// Endpoints and shapes verified against notifiarr.wiki (Passthrough
// integration) and the vendor's own client, github.com/Notifiarr/notifiarr
// (`pkg/website/website_routes.go`, `pkg/website/website.go`).
// ---------------------------------------------------------------------------

const BASE_URL: &str = "https://notifiarr.com";
/// `notifiRoute + "/passthrough"`. The wiki documents the API key as a path
/// segment; the vendor's client sends `X-API-Key` on every notification route.
/// Both are sent, so the plugin works whichever one Notifiarr reads.
const PASSTHROUGH_PATH: &str = "/api/v1/notification/passthrough";
/// The endpoint Sonarr's own Notifiarr connection posts to
/// (`NotifiarrProxy.cs:29`).
const SONARR_PATH: &str = "/api/v1/notification/sonarr";
/// `ValidateRoute.Path(EventUser)` — a `GET` whose only job is to say whether
/// the key is accepted (`website.go:334-368`). Used as a Test-time probe;
/// Sonarr has no equivalent and cannot tell a bad key from a disabled
/// integration.
const VALIDATE_PATH: &str = "/api/v1/user/validate?event=user";
/// `website_routes.go:24` — `APIKeyLength = 36`.
const API_KEY_LENGTH: usize = 36;

// ---------------------------------------------------------------------------
// Discord embed limits
//
// The passthrough payload becomes a Discord embed inside Notifiarr, so Discord's
// documented caps are the ones that decide whether the message survives; the
// wiki states the 25-field cap directly.
// ---------------------------------------------------------------------------

const EMBED_TITLE_LIMIT: usize = 256;
const EMBED_DESCRIPTION_LIMIT: usize = 4096;
const EMBED_FIELD_COUNT_LIMIT: usize = 25;
const EMBED_FIELD_NAME_LIMIT: usize = 256;
const EMBED_FIELD_VALUE_LIMIT: usize = 1024;
const EMBED_FOOTER_LIMIT: usize = 2048;
const EMBED_CONTENT_LIMIT: usize = 2000;
const EMBED_TOTAL_LIMIT: usize = 6000;
/// Sonarr's own cut for a synopsis (`Discord.cs:79`), reused so the two channels
/// render the same overview.
const OVERVIEW_LIMIT: usize = 300;

// ---------------------------------------------------------------------------
// Colours
//
// `DiscordColors.cs`, expressed as the 6-digit hex string the passthrough schema
// asks for instead of Discord's decimal integer.
// ---------------------------------------------------------------------------

/// `DiscordColors.Danger` (15749200).
const COLOR_DANGER: u32 = 0x00F0_5050;
/// `DiscordColors.Success` (2605644).
const COLOR_SUCCESS: u32 = 0x0027_C24C;
/// `DiscordColors.Warning` (16753920).
const COLOR_WARNING: u32 = 0x00FF_A500;
/// `DiscordColors.Standard` (16761392).
const COLOR_STANDARD: u32 = 0x00FF_C230;
/// `DiscordColors.Upgrade` (4089856).
const COLOR_UPGRADE: u32 = 0x003E_6800;

const INTEGRATION_PASSTHROUGH: &str = "passthrough";
const INTEGRATION_SONARR: &str = "sonarr";
const DEFAULT_NOTIFICATION_NAME: &str = "Scryer";

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

/// Built here rather than through `notify_common::build_notification_descriptor`
/// because that helper cannot express `default_base_url` or `event_options`, and
/// Notifiarr relays every one of Sonarr's filterable events.
fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Notifiarr".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            // There is no operator-settable server: Notifiarr is a hosted
            // service on exactly one origin.
            default_base_url: Some(BASE_URL.to_string()),
            allowed_hosts: vec!["notifiarr.com".to_string()],
            capabilities: NotificationCapabilities {
                supports_rich_text: true,
                supports_images: true,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![
                    NotificationDeliveryMode::Webhook,
                    NotificationDeliveryMode::Aggregator,
                ],
                payload_formats: vec![
                    NotificationPayloadFormat::StructuredJson,
                    NotificationPayloadFormat::RichEmbed,
                ],
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
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some(
                "Your Notifiarr API key (36 characters), from notifiarr.com -> Profile -> API Key.",
            ),
        ),
        select_field(
            "integration",
            "Notifiarr Integration",
            Some(INTEGRATION_PASSTHROUGH),
            &[
                (
                    INTEGRATION_PASSTHROUGH,
                    "Passthrough (all events, all media types)",
                ),
                (
                    INTEGRATION_SONARR,
                    "Sonarr integration (TV only, Sonarr-compatible payload)",
                ),
            ],
        ),
        field(
            "channel_id",
            "Discord Channel ID",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Numeric Discord channel id the Passthrough integration posts to. Required by the passthrough integration; ignored by the Sonarr integration, which uses the channel picker on notifiarr.com.",
            ),
        ),
        field(
            "notification_name",
            "Notification Name",
            ConfigFieldType::String,
            false,
            Some(DEFAULT_NOTIFICATION_NAME),
            Some(
                "The app name Notifiarr groups these passthrough notifications under. Defaults to Scryer.",
            ),
        ),
        field(
            "instance_name",
            "Instance Name",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Distinguishes this Scryer from another one in the same Notifiarr account. Sent as instanceName to the Sonarr integration and shown in the passthrough footer.",
            ),
        ),
        connection_field(
            "application_url",
            "Application URL",
            false,
            None,
            Some(
                "Externally reachable URL of this Scryer, sent as applicationUrl. Scryer's notification contract has no carrier for it, so it is configured here.",
            ),
        ),
        field(
            "ping_user",
            "Ping User ID",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Numeric Discord user id to mention on every passthrough notification. Text rather than a number because a Discord snowflake does not survive a JSON number.",
            ),
        ),
        field(
            "ping_role",
            "Ping Role ID",
            ConfigFieldType::String,
            false,
            None,
            Some("Numeric Discord role id to mention on every passthrough notification."),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Integration {
    Passthrough,
    Sonarr,
}

impl Integration {
    fn label(self) -> &'static str {
        match self {
            Self::Passthrough => INTEGRATION_PASSTHROUGH,
            Self::Sonarr => INTEGRATION_SONARR,
        }
    }
}

/// Everything the renderer and the sender need from configuration, resolved
/// once per send so every builder below is a pure function of
/// `(request, settings)` and unit-testable without a host.
#[derive(Debug, Clone)]
struct Settings {
    api_key: String,
    integration: Integration,
    channel_id: Option<i64>,
    notification_name: String,
    instance_name: Option<String>,
    application_url: Option<String>,
    ping_user: Option<i64>,
    ping_role: Option<i64>,
}

impl Settings {
    /// `strict` is the Test-time posture. Anything Notifiarr itself will decide
    /// (the key's validity, whether the channel exists) is left to Notifiarr;
    /// what is checked here is only what the plugin can know locally, and at
    /// send time a local doubt degrades to a warning rather than dropping a
    /// notification the operator would otherwise have received.
    fn from_config(strict: bool) -> Result<(Self, Vec<String>), PluginError> {
        let mut warnings = Vec::new();

        let api_key = config_value("api_key").ok_or_else(|| {
            plugin_error(
                PluginErrorCode::InvalidConfig,
                "api_key is not configured; paste the API key from your Notifiarr profile"
                    .to_string(),
                None,
            )
        })?;
        // `website_routes.go:136` refuses a key of the wrong length before it
        // ever reaches the network. A warning rather than an error, because the
        // key format is Notifiarr's to change and a wrong guess here would
        // silence a working channel.
        if api_key.chars().count() != API_KEY_LENGTH {
            warnings.push(format!(
                "api_key is {} characters; Notifiarr's own client requires exactly {API_KEY_LENGTH}",
                api_key.chars().count()
            ));
        }

        let integration = match config_value("integration").as_deref() {
            None | Some(INTEGRATION_PASSTHROUGH) => Integration::Passthrough,
            Some(INTEGRATION_SONARR) => Integration::Sonarr,
            // An unrecognised option is a forward-compatible config, not a
            // failure: fall back to the default and say so.
            Some(other) => {
                warnings.push(format!(
                    "integration '{other}' is not one of '{INTEGRATION_PASSTHROUGH}' or '{INTEGRATION_SONARR}'; using '{INTEGRATION_PASSTHROUGH}'"
                ));
                Integration::Passthrough
            }
        };

        let channel_id = parse_snowflake("channel_id", &mut warnings);
        if integration == Integration::Passthrough && channel_id.is_none() {
            // `discord.ids.channel` is Required in the passthrough schema, so
            // this is a setting the operator has to fill in — the configuration
            // lane, at test time and at send time alike.
            return Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                "channel_id is required by Notifiarr's passthrough integration: paste the numeric Discord channel id the notification should land in".to_string(),
                None,
            ));
        }

        let application_url = validated_application_url(strict, &mut warnings)?;

        Ok((
            Self {
                api_key,
                integration,
                channel_id,
                notification_name: config_value("notification_name")
                    .unwrap_or_else(|| DEFAULT_NOTIFICATION_NAME.to_string()),
                instance_name: config_value("instance_name"),
                application_url,
                ping_user: parse_snowflake("ping_user", &mut warnings),
                ping_role: parse_snowflake("ping_role", &mut warnings),
            },
            warnings,
        ))
    }

    #[cfg(test)]
    fn passthrough() -> Self {
        Self {
            api_key: "0123456789abcdef0123456789abcdef0123".to_string(),
            integration: Integration::Passthrough,
            channel_id: Some(910_000_000_000_000_001),
            notification_name: DEFAULT_NOTIFICATION_NAME.to_string(),
            instance_name: None,
            application_url: None,
            ping_user: None,
            ping_role: None,
        }
    }

    #[cfg(test)]
    fn sonarr() -> Self {
        Self {
            integration: Integration::Sonarr,
            channel_id: None,
            ..Self::passthrough()
        }
    }

    fn instance(&self, req: &PluginNotificationRequest) -> String {
        self.instance_name
            .clone()
            .unwrap_or_else(|| req.app.name.clone())
    }
}

/// Discord ids are 64-bit snowflakes. They arrive as text so a JSON number
/// cannot round them, and a value that is not a number at all is a warning plus
/// an omitted field rather than a dropped notification.
fn parse_snowflake(key: &str, warnings: &mut Vec<String>) -> Option<i64> {
    let raw = config_value(key)?;
    match raw.parse::<i64>() {
        Ok(value) if value > 0 => Some(value),
        _ => {
            warnings.push(format!(
                "{key} must be a numeric Discord id; '{raw}' was ignored"
            ));
            None
        }
    }
}

fn validated_application_url(
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Option<String>, PluginError> {
    let Some(url) = config_value("application_url") else {
        return Ok(None);
    };
    let lowercase = url.to_ascii_lowercase();
    if lowercase.starts_with("http://") || lowercase.starts_with("https://") {
        return Ok(Some(url));
    }
    if strict {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "application_url must be an http(s) URL".to_string(),
            Some(format!("configured value: {url}")),
        ));
    }
    warnings.push("application_url is not an http(s) URL and was omitted".to_string());
    Ok(None)
}

// ---------------------------------------------------------------------------
// Passthrough payload
//
// https://notifiarr.wiki/pages/integrations/passthrough/
//   { notification: { update, name, event },
//     discord: { color, ping: { pingUser, pingRole },
//                images: { thumbnail, image },
//                text: { title, icon, content, description, fields[], footer },
//                ids: { channel } } }
// ---------------------------------------------------------------------------

fn build_passthrough_payload(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Value {
    let mut notification = Map::new();
    notification.insert("update".to_string(), json!(false));
    notification.insert("name".to_string(), json!(settings.notification_name));
    if let Some(event_id) = trimmed(req.event_id.as_deref()) {
        notification.insert("event".to_string(), json!(event_id));
    }

    let mut text = Map::new();
    text.insert("title".to_string(), json!(heading(req)));
    if let Some(description) = description(req) {
        text.insert("description".to_string(), json!(description));
    }
    if req.is_test || req.event_type == NotificationEventType::Test {
        // Sonarr's own test notifications are a plain content line; Discord
        // shows `content` in the toast, which is the cheapest proof the channel
        // reached a human.
        text.insert("content".to_string(), json!(test_message(req, settings)));
    }
    let fields = event_fields(req);
    if !fields.is_empty() {
        text.insert("fields".to_string(), Value::Array(fields));
    }
    text.insert("footer".to_string(), json!(footer(req, settings)));

    let mut discord = Map::new();
    discord.insert("color".to_string(), json!(color_hex(req)));
    discord.insert(
        "ids".to_string(),
        json!({ "channel": settings.channel_id.unwrap_or_default() }),
    );
    if settings.ping_user.is_some() || settings.ping_role.is_some() {
        let mut ping = Map::new();
        if let Some(user) = settings.ping_user {
            ping.insert("pingUser".to_string(), json!(user));
        }
        if let Some(role) = settings.ping_role {
            ping.insert("pingRole".to_string(), json!(role));
        }
        discord.insert("ping".to_string(), Value::Object(ping));
    }
    let mut images = Map::new();
    if let Some(poster) = title_image(req, |title| title.poster_url.as_deref()) {
        images.insert("thumbnail".to_string(), json!(poster));
    }
    if let Some(fanart) = title_image(req, |title| title.background_url.as_deref()) {
        images.insert("image".to_string(), json!(fanart));
    }
    if !images.is_empty() {
        discord.insert("images".to_string(), Value::Object(images));
    }
    discord.insert("text".to_string(), Value::Object(text));

    let mut payload = Value::Object(Map::from_iter([
        ("notification".to_string(), Value::Object(notification)),
        ("discord".to_string(), Value::Object(discord)),
    ]));
    enforce_embed_limits(&mut payload, warnings);
    payload
}

fn test_message(req: &PluginNotificationRequest, settings: &Settings) -> String {
    match trimmed(req.occurred_at.as_deref()) {
        Some(occurred_at) => format!(
            "Test message from {} posted at {occurred_at}",
            settings.instance(req)
        ),
        None => format!("Test message from {}", settings.instance(req)),
    }
}

fn footer(req: &PluginNotificationRequest, settings: &Settings) -> String {
    let mut footer = format!("{} {}", settings.instance(req), req.app.version);
    if let Some(url) = settings.application_url.as_deref() {
        footer.push_str(" - ");
        footer.push_str(url);
    }
    footer
}

fn title_image<'a>(
    req: &'a PluginNotificationRequest,
    pick_url: impl Fn(&'a PluginNotificationTitle) -> Option<&'a str>,
) -> Option<&'a str> {
    trimmed(req.title.as_ref().and_then(pick_url))
}

// ---------------------------------------------------------------------------
// Passthrough rendering
// ---------------------------------------------------------------------------

fn summary(req: &PluginNotificationRequest) -> String {
    match trimmed(Some(req.summary_title.as_str())) {
        Some(summary) => summary.to_string(),
        None => req.app.name.clone(),
    }
}

/// `Discord.GetTitle` (`Discord.cs:711-737`) on Scryer's contract: the title's
/// name plus whatever episode detail the contract carries.
fn heading(req: &PluginNotificationRequest) -> String {
    match req.event_type {
        NotificationEventType::HealthIssue => health_source(req).unwrap_or_else(|| summary(req)),
        NotificationEventType::HealthRestored => match health_source(req) {
            Some(source) => format!("Health Issue Resolved: {source}"),
            None => summary(req),
        },
        NotificationEventType::ApplicationUpdate | NotificationEventType::Test => summary(req),
        _ => {
            let name = req
                .title
                .as_ref()
                .and_then(|title| trimmed(Some(title.name.as_str())))
                .map(str::to_string)
                .unwrap_or_else(|| summary(req));
            match episode_detail(req) {
                Some(detail) => format!("{name} - {detail}"),
                None => name,
            }
        }
    }
}

fn episode_detail(req: &PluginNotificationRequest) -> Option<String> {
    if let Some(display) = req
        .episode
        .as_ref()
        .and_then(|episode| trimmed(episode.display.as_deref()))
    {
        return Some(display.to_string());
    }

    let episodes = episode_list(req);
    let first = episodes.first().copied()?;

    let titles: Vec<&str> = episodes
        .iter()
        .filter_map(|episode| trimmed(episode.title.as_deref()))
        .collect();
    let titles = titles.join(" + ");

    // Sonarr's daily-series branch keys off `SeriesTypes.Daily`; the contract has
    // no series type, so the observable stand-in is an episode with an air date
    // and no episode number.
    if first.episode_number.is_none()
        && let Some(air_date) = trimmed(first.air_date.as_deref())
    {
        return Some(if titles.is_empty() {
            air_date.to_string()
        } else {
            format!("{air_date} - {titles}")
        });
    }

    let numbers: String = episodes
        .iter()
        .filter_map(|episode| trimmed(episode.episode_number.as_deref()))
        .map(|number| match number.parse::<u32>() {
            Ok(parsed) => format!("x{parsed:02}"),
            Err(_) => format!("x{number}"),
        })
        .collect();
    let season = trimmed(first.season_number.as_deref());

    match (season, numbers.is_empty(), titles.is_empty()) {
        (Some(season), false, true) => Some(format!("{season}{numbers}")),
        (Some(season), false, false) => Some(format!("{season}{numbers} - {titles}")),
        (_, _, false) => Some(titles),
        _ => None,
    }
}

fn episode_list(req: &PluginNotificationRequest) -> Vec<&PluginNotificationEpisode> {
    if req.episodes.is_empty() {
        req.episode.iter().collect()
    } else {
        req.episodes.iter().collect()
    }
}

/// The short event label Sonarr puts in its Discord `Description`, generalised:
/// Sonarr's wording is series-specific because Sonarr only has series, and
/// Scryer carries a facet.
fn event_label(req: &PluginNotificationRequest) -> String {
    let episodic = is_episodic(req);
    match req.event_type {
        NotificationEventType::Grab => pick(episodic, "Episode Grabbed", "Grabbed"),
        NotificationEventType::Download => {
            if is_failure(req) {
                "Download Failed".to_string()
            } else if is_upgrade(req) {
                pick(episodic, "Episode Upgraded", "Upgraded")
            } else {
                pick(episodic, "Episode Imported", "Imported")
            }
        }
        NotificationEventType::Upgrade => pick(episodic, "Episode Upgraded", "Upgraded"),
        NotificationEventType::ImportComplete => "Import Complete".to_string(),
        NotificationEventType::ImportRejected => "Import Rejected".to_string(),
        NotificationEventType::Rename => "Renamed".to_string(),
        NotificationEventType::FileDeleted => pick(episodic, "Episode Deleted", "File Deleted"),
        NotificationEventType::FileDeletedForUpgrade => pick(
            episodic,
            "Episode Deleted for Upgrade",
            "File Deleted for Upgrade",
        ),
        NotificationEventType::TitleAdded => pick(episodic, "Series Added", "Added"),
        NotificationEventType::TitleDeleted => pick(episodic, "Series Deleted", "Deleted"),
        NotificationEventType::ManualInteractionRequired => "Manual interaction needed".to_string(),
        NotificationEventType::PostProcessingCompleted => "Post-processing Complete".to_string(),
        NotificationEventType::SubtitleDownloaded => "Subtitle Downloaded".to_string(),
        NotificationEventType::SubtitleSearchFailed => "Subtitle Search Failed".to_string(),
        NotificationEventType::MediaRequestSubmitted => "Media Request Submitted".to_string(),
        NotificationEventType::MediaRequestApproved => "Media Request Approved".to_string(),
        NotificationEventType::MediaRequestRejected => "Media Request Rejected".to_string(),
        NotificationEventType::MediaRequestCanceled => "Media Request Canceled".to_string(),
        NotificationEventType::HealthIssue => "Health Issue".to_string(),
        NotificationEventType::HealthRestored => "Health Issue Resolved".to_string(),
        NotificationEventType::ApplicationUpdate => "Application Updated".to_string(),
        NotificationEventType::Test => "Test".to_string(),
    }
}

fn pick(episodic: bool, episodic_label: &str, neutral_label: &str) -> String {
    if episodic {
        episodic_label.to_string()
    } else {
        neutral_label.to_string()
    }
}

fn description(req: &PluginNotificationRequest) -> Option<String> {
    let message = req.summary_message.trim();
    match req.event_type {
        NotificationEventType::HealthIssue => Some(
            req.health
                .as_ref()
                .and_then(|health| trimmed(health.message.as_deref()).map(str::to_string))
                .unwrap_or_else(|| message.to_string()),
        )
        .filter(|value| !value.is_empty()),
        NotificationEventType::HealthRestored => {
            let detail = req
                .health
                .as_ref()
                .and_then(|health| trimmed(health.message.as_deref()).map(str::to_string))
                .unwrap_or_else(|| message.to_string());
            (!detail.is_empty()).then(|| format!("The following issue is now resolved: {detail}"))
        }
        NotificationEventType::ApplicationUpdate => req
            .application_update
            .as_ref()
            .and_then(|update| trimmed(update.summary.as_deref()).map(str::to_string))
            .or_else(|| (!message.is_empty()).then(|| message.to_string())),
        _ => {
            let label = event_label(req);
            if message.is_empty() || message == label {
                Some(label)
            } else {
                Some(format!("{label}\n{message}"))
            }
        }
    }
}

/// `DiscordColors` per event, with Scryer's `severity` as an override Sonarr has
/// no equivalent for. A warning never downgrades an already-red event.
fn color(req: &PluginNotificationRequest) -> u32 {
    let base = match req.event_type {
        NotificationEventType::Grab
        | NotificationEventType::ManualInteractionRequired
        | NotificationEventType::ApplicationUpdate
        | NotificationEventType::Rename
        | NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestCanceled
        | NotificationEventType::Test => COLOR_STANDARD,
        NotificationEventType::Download => {
            if is_failure(req) {
                COLOR_DANGER
            } else if is_upgrade(req) {
                COLOR_UPGRADE
            } else {
                COLOR_SUCCESS
            }
        }
        NotificationEventType::Upgrade => COLOR_UPGRADE,
        NotificationEventType::ImportComplete
        | NotificationEventType::TitleAdded
        | NotificationEventType::HealthRestored
        | NotificationEventType::PostProcessingCompleted
        | NotificationEventType::SubtitleDownloaded
        | NotificationEventType::MediaRequestApproved => COLOR_SUCCESS,
        NotificationEventType::FileDeleted
        | NotificationEventType::FileDeletedForUpgrade
        | NotificationEventType::TitleDeleted
        | NotificationEventType::ImportRejected
        | NotificationEventType::SubtitleSearchFailed
        | NotificationEventType::MediaRequestRejected => COLOR_DANGER,
        NotificationEventType::HealthIssue => COLOR_WARNING,
    };

    match req.severity {
        Some(NotificationSeverity::Error) => COLOR_DANGER,
        Some(NotificationSeverity::Warning) if base != COLOR_DANGER => COLOR_WARNING,
        _ => base,
    }
}

/// The passthrough schema asks for a "6 digit HTML color code", not Discord's
/// decimal integer.
fn color_hex(req: &PluginNotificationRequest) -> String {
    format!("{:06X}", color(req))
}

fn event_fields(req: &PluginNotificationRequest) -> Vec<Value> {
    let mut fields = Vec::new();
    match req.event_type {
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            push_field(&mut fields, "Reason", Some(event_label(req)), true);
            push_field(&mut fields, "File", deleted_path(req), false);
        }
        NotificationEventType::ApplicationUpdate => {
            let update = req.application_update.as_ref();
            push_field(
                &mut fields,
                "Previous Version",
                update.and_then(|update| update.current_version.clone()),
                true,
            );
            push_field(
                &mut fields,
                "New Version",
                update.and_then(|update| update.target_version.clone()),
                true,
            );
        }
        NotificationEventType::HealthIssue | NotificationEventType::HealthRestored => {
            let health = req.health.as_ref();
            push_field(
                &mut fields,
                "Type",
                health.and_then(|health| health.code.clone()),
                true,
            );
            push_field(
                &mut fields,
                "Level",
                health.and_then(|health| health.severity.clone()),
                true,
            );
        }
        NotificationEventType::Rename => {
            for update in renamed_paths(req).into_iter().take(5) {
                push_field(&mut fields, "Renamed", Some(update), false);
            }
        }
        NotificationEventType::Test => {}
        _ => {
            push_field(&mut fields, "Quality", quality(req), true);
            push_field(&mut fields, "Group", release_group(req), true);
            push_field(&mut fields, "Size", size_bytes(req).map(format_bytes), true);
            push_field(&mut fields, "Codecs", codecs(req), true);
            push_field(
                &mut fields,
                "Languages",
                media_languages(req, |file| &file.audio_languages),
                true,
            );
            push_field(
                &mut fields,
                "Subtitles",
                media_languages(req, |file| &file.subtitle_languages),
                true,
            );
            push_field(&mut fields, "Indexer", indexer(req), true);
            push_field(
                &mut fields,
                "Download Client",
                req.download
                    .as_ref()
                    .and_then(|download| download.client_name.clone()),
                true,
            );
            push_field(
                &mut fields,
                "Custom Formats",
                custom_formats(req).map(|(names, _)| names),
                true,
            );
            push_field(
                &mut fields,
                "Custom Format Score",
                custom_formats(req).map(|(_, score)| score.to_string()),
                true,
            );
            push_field(&mut fields, "Release", release_title(req), false);
            push_field(&mut fields, "Overview", overview(req), false);
            push_field(&mut fields, "Destination", destination_path(req), false);
            push_field(&mut fields, "Links", links_string(req), false);
        }
    }
    fields
}

/// The passthrough field object is `{title, text, inline}` — not Discord's
/// `{name, value, inline}` — and an entry with an empty half is dropped rather
/// than sent.
fn push_field(fields: &mut Vec<Value>, title: &str, text: Option<String>, inline: bool) {
    let Some(text) = text else { return };
    if text.trim().is_empty() {
        return;
    }
    fields.push(json!({ "title": title, "text": text, "inline": inline }));
}

fn is_episodic(req: &PluginNotificationRequest) -> bool {
    facet(req).is_some_and(|facet| matches!(facet.as_str(), "series" | "anime" | "tv" | "show"))
}

fn facet(req: &PluginNotificationRequest) -> Option<String> {
    req.title
        .as_ref()
        .map(|title| title.facet.trim().to_ascii_lowercase())
        .filter(|facet| !facet.is_empty())
}

/// `NotificationEventType::Download` is not Sonarr's `OnDownload`: Scryer's
/// dispatcher maps a **failed** download onto it
/// (`crates/scryer-application/src/notifications/dispatcher.rs:34,418-448`, both
/// `release-0.19.8` and `release-next`), and `enrich_notification`
/// (`dispatcher.rs:879-916`) stamps `severity: Error` on it.
fn is_failure(req: &PluginNotificationRequest) -> bool {
    if matches!(req.severity, Some(NotificationSeverity::Error)) {
        return true;
    }
    if let Some(status) = req
        .download
        .as_ref()
        .and_then(|download| download.status.as_deref())
        && matches!(
            status.to_ascii_lowercase().as_str(),
            "failed" | "failure" | "error"
        )
    {
        return true;
    }
    req.summary_title.to_ascii_lowercase().contains("failed")
}

fn is_upgrade(req: &PluginNotificationRequest) -> bool {
    req.event_type == NotificationEventType::Upgrade
        || req.import.as_ref().is_some_and(|import| import.upgrade)
}

fn health_source(req: &PluginNotificationRequest) -> Option<String> {
    req.health.as_ref().and_then(|health| {
        trimmed(health.code.as_deref())
            .or_else(|| trimmed(health.status.as_deref()))
            .map(str::to_string)
    })
}

// ---------------------------------------------------------------------------
// Field values shared by both payloads
// ---------------------------------------------------------------------------

fn release(req: &PluginNotificationRequest) -> Option<&PluginNotificationRelease> {
    req.release.as_ref()
}

fn quality(req: &PluginNotificationRequest) -> Option<String> {
    release(req)
        .and_then(|release| release.quality.clone())
        .or_else(|| req.media_files.iter().find_map(|file| file.quality.clone()))
        .and_then(|quality| trimmed(Some(quality.as_str())).map(str::to_string))
}

fn release_group(req: &PluginNotificationRequest) -> Option<String> {
    release(req)
        .and_then(|release| release.release_group.clone())
        .or_else(|| {
            req.media_files
                .iter()
                .find_map(|file| file.release_group.clone())
        })
        .and_then(|group| trimmed(Some(group.as_str())).map(str::to_string))
}

fn indexer(req: &PluginNotificationRequest) -> Option<String> {
    release(req)
        .and_then(|release| {
            release
                .indexer
                .clone()
                .or_else(|| release.source_hint.clone())
        })
        .and_then(|indexer| trimmed(Some(indexer.as_str())).map(str::to_string))
}

fn release_title(req: &PluginNotificationRequest) -> Option<String> {
    release(req)
        .and_then(|release| release.source_title.clone())
        .or_else(|| {
            req.media_files
                .iter()
                .find_map(|file| file.scene_name.clone())
        })
        .or_else(|| {
            req.import
                .as_ref()
                .and_then(|import| import.source_title.clone())
        })
        .or_else(|| {
            req.download
                .as_ref()
                .and_then(|download| download.title.clone())
        })
        .and_then(|title| trimmed(Some(title.as_str())).map(str::to_string))
}

/// Sonarr prefers the release size and falls back to the sum of the imported
/// files (`Discord.cs:317`).
fn size_bytes(req: &PluginNotificationRequest) -> Option<i64> {
    if let Some(size) = req
        .download
        .as_ref()
        .and_then(|download| download.size_bytes)
        .filter(|size| *size > 0)
    {
        return Some(size);
    }
    let summed: i64 = req
        .media_files
        .iter()
        .filter_map(|file| file.size_bytes)
        .sum();
    (summed > 0).then_some(summed)
}

fn codecs(req: &PluginNotificationRequest) -> Option<String> {
    let file = req.media_files.first()?;
    let audio = match (file.audio_codec.as_deref(), file.audio_channels.as_deref()) {
        (Some(codec), Some(channels)) => Some(format!("{codec} {channels}")),
        (Some(codec), None) => Some(codec.to_string()),
        (None, Some(channels)) => Some(channels.to_string()),
        (None, None) => None,
    };
    match (file.video_codec.as_deref(), audio) {
        (Some(video), Some(audio)) => Some(format!("{video} / {audio}")),
        (Some(video), None) => Some(video.to_string()),
        (None, Some(audio)) => Some(audio),
        (None, None) => None,
    }
}

fn media_languages(
    req: &PluginNotificationRequest,
    take: impl Fn(&PluginNotificationMediaFile) -> &Vec<String>,
) -> Option<String> {
    let mut values: Vec<String> = Vec::new();
    for file in &req.media_files {
        for value in take(file) {
            let value = value.trim();
            if !value.is_empty() && !values.iter().any(|existing| existing == value) {
                values.push(value.to_string());
            }
        }
    }
    (!values.is_empty()).then(|| values.join("/"))
}

fn custom_formats(req: &PluginNotificationRequest) -> Option<(String, i32)> {
    let scores = &release(req)?.custom_scores;
    (!scores.is_empty()).then(|| {
        (
            scores
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            scores.values().sum(),
        )
    })
}

fn overview(req: &PluginNotificationRequest) -> Option<String> {
    let overview = req
        .episode
        .as_ref()
        .and_then(|episode| episode.overview.clone())
        .or_else(|| {
            req.episodes
                .first()
                .and_then(|episode| episode.overview.clone())
        })
        .or_else(|| req.title.as_ref().and_then(|title| title.overview.clone()))?;
    let overview = overview.trim();
    if overview.is_empty() {
        return None;
    }
    Some(if overview.chars().count() <= OVERVIEW_LIMIT {
        overview.to_string()
    } else {
        let head: String = overview.chars().take(OVERVIEW_LIMIT).collect();
        format!("{head}...")
    })
}

fn destination_path(req: &PluginNotificationRequest) -> Option<String> {
    req.import
        .as_ref()
        .and_then(|import| trimmed(import.dest_path.as_deref()).map(str::to_string))
        .or_else(|| req.media_files.first().map(|file| file.path.clone()))
}

fn deleted_path(req: &PluginNotificationRequest) -> Option<String> {
    if let Some(file) = req.file.as_ref() {
        if let Some(update) = file.media_updates.iter().find(|update| {
            update.update_type == scryer_plugin_sdk::NotificationMediaUpdateType::Deleted
        }) {
            return Some(update.path.clone());
        }
        if let Some(update) = file.media_updates.first() {
            return Some(update.path.clone());
        }
        if let Some(path) = trimmed(file.primary_path.as_deref()) {
            return Some(path.to_string());
        }
    }
    req.media_files.first().map(|file| file.path.clone())
}

fn renamed_paths(req: &PluginNotificationRequest) -> Vec<String> {
    req.media_files
        .iter()
        .map(|file| match trimmed(file.previous_path.as_deref()) {
            Some(previous) => format!("{previous} -> {}", file.path),
            None => file.path.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// `Discord.GetLinksString` (`Discord.cs:690-709`) generalised to Scryer's
/// facets and external-id set: Sonarr only ever has a series, Scryer carries a
/// facet and a wider id set.
fn metadata_links(req: &PluginNotificationRequest) -> Vec<(&'static str, String)> {
    let Some(title) = req.title.as_ref() else {
        return Vec::new();
    };
    let ids = &title.external_ids;

    let tvdb = external_id(ids.tvdb_id.as_deref(), ids, "tvdb");
    let tmdb = external_id(ids.tmdb_id.as_deref(), ids, "tmdb");
    let imdb = external_id(ids.imdb_id.as_deref(), ids, "imdb");
    let tvmaze = external_id(ids.tvmaze_id.as_deref(), ids, "tvmaze");

    let mut links: Vec<(&'static str, String)> = Vec::new();
    if is_episodic(req) {
        if let Some(id) = &tvdb {
            links.push((
                "The TVDB",
                format!("https://thetvdb.com/?tab=series&id={id}"),
            ));
            links.push((
                "Trakt",
                format!("https://trakt.tv/search/tvdb/{id}?id_type=show"),
            ));
        }
        if let Some(id) = &tvmaze {
            links.push(("TVmaze", format!("https://www.tvmaze.com/shows/{id}")));
        }
    } else if let Some(id) = &tmdb {
        links.push(("TMDB", format!("https://www.themoviedb.org/movie/{id}")));
        links.push((
            "Trakt",
            format!("https://trakt.tv/search/tmdb/{id}?id_type=movie"),
        ));
    }
    if let Some(id) = &imdb {
        links.push(("IMDb", format!("https://imdb.com/title/{id}/")));
    }

    // An anime id present is evidence enough; the facet does not have to say so.
    if let Some(id) = external_id(ids.anidb_id.as_deref(), ids, "anidb") {
        links.push(("AniDB", format!("https://anidb.net/anime/{id}")));
    }
    if let Some(id) = external_id(ids.anilist_ids.first().map(String::as_str), ids, "anilist") {
        links.push(("AniList", format!("https://anilist.co/anime/{id}")));
    }
    if let Some(id) = external_id(ids.mal_ids.first().map(String::as_str), ids, "mal") {
        links.push(("MyAnimeList", format!("https://myanimelist.net/anime/{id}")));
    }
    if let Some(id) = external_id(ids.kitsu_ids.first().map(String::as_str), ids, "kitsu") {
        links.push(("Kitsu", format!("https://kitsu.app/anime/{id}")));
    }

    links
}

fn external_id(
    typed: Option<&str>,
    ids: &scryer_plugin_sdk::PluginNotificationExternalIds,
    source: &str,
) -> Option<String> {
    trimmed(typed).map(str::to_string).or_else(|| {
        ids.by_source
            .get(source)
            .and_then(|values| values.first())
            .and_then(|id| trimmed(Some(id.as_str())).map(str::to_string))
    })
}

fn links_string(req: &PluginNotificationRequest) -> Option<String> {
    let links = metadata_links(req);
    (!links.is_empty()).then(|| {
        links
            .iter()
            .map(|(label, url)| format!("[{label}]({url})"))
            .collect::<Vec<_>>()
            .join(" / ")
    })
}

// ---------------------------------------------------------------------------
// Sonarr-compatible payload
//
// `Notifications/Webhook/WebhookBase.cs` on Scryer's contract. Serialisation
// rules from `NzbDrone.Common/Serializer/Newtonsoft.Json/Json.cs`: camelCase
// members, nulls omitted, enums as camelCase strings — except `eventType`,
// which pins `DefaultNamingStrategy` and is therefore PascalCase
// (`WebhookEventType.cs`).
// ---------------------------------------------------------------------------

/// Which `WebhookEventType` a Scryer event may claim, or `None` when Sonarr's
/// closed enum has no member for it.
fn sonarr_event_type(req: &PluginNotificationRequest) -> Option<&'static str> {
    match req.event_type {
        NotificationEventType::Grab => Some("Grab"),
        // `BuildOnImportCompletePayload` also stamps `Download`
        // (`WebhookBase.cs:93`), so all three import shapes share it.
        NotificationEventType::Upgrade | NotificationEventType::ImportComplete => Some("Download"),
        // Scryer's `Download` is a *failed* download. Claiming Sonarr's
        // `Download` there would tell Notifiarr an episode imported. The
        // truthful neighbour in Sonarr's vocabulary is the payload that carries
        // `downloadStatus`/`downloadStatusMessages`.
        NotificationEventType::Download => Some(if is_failure(req) {
            "ManualInteractionRequired"
        } else {
            "Download"
        }),
        NotificationEventType::ImportRejected
        | NotificationEventType::ManualInteractionRequired => Some("ManualInteractionRequired"),
        NotificationEventType::Rename => Some("Rename"),
        NotificationEventType::TitleAdded => Some("SeriesAdd"),
        NotificationEventType::TitleDeleted => Some("SeriesDelete"),
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            Some("EpisodeFileDelete")
        }
        NotificationEventType::HealthIssue => Some("Health"),
        NotificationEventType::HealthRestored => Some("HealthRestored"),
        NotificationEventType::ApplicationUpdate => Some("ApplicationUpdate"),
        NotificationEventType::Test => Some("Test"),
        // Scryer-only events. Notifiarr's Sonarr integration has no schema for
        // them at all, so the honest answer is to say so rather than to invent
        // an event type it will reject.
        NotificationEventType::PostProcessingCompleted
        | NotificationEventType::SubtitleDownloaded
        | NotificationEventType::SubtitleSearchFailed
        | NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestApproved
        | NotificationEventType::MediaRequestRejected
        | NotificationEventType::MediaRequestCanceled => None,
    }
}

fn build_sonarr_payload(
    req: &PluginNotificationRequest,
    settings: &Settings,
    warnings: &mut Vec<String>,
) -> Result<Value, PluginError> {
    let Some(event_type) = sonarr_event_type(req) else {
        return Err(plugin_error(
            PluginErrorCode::Unsupported,
            format!(
                "Notifiarr's Sonarr integration has no event for '{}'. Switch the integration setting to '{INTEGRATION_PASSTHROUGH}' to receive this event.",
                event_label(req)
            ),
            Some(format!("event_type={:?}", req.event_type)),
        ));
    };

    if req.title.is_some() && !is_episodic(req) {
        warnings.push(format!(
            "Notifiarr's Sonarr integration only models series, so the {} '{}' was sent as a series; the passthrough integration renders it truthfully",
            facet(req).unwrap_or_else(|| "title".to_string()),
            heading(req)
        ));
    }
    if event_type == "ManualInteractionRequired"
        && req.event_type == NotificationEventType::Download
    {
        warnings.push(
            "a failed download was sent as Sonarr's ManualInteractionRequired event; Sonarr's webhook schema has no failed-download event".to_string(),
        );
    }

    let mut payload = Map::new();
    payload.insert("eventType".to_string(), json!(event_type));
    payload.insert("instanceName".to_string(), json!(settings.instance(req)));
    if let Some(url) = settings.application_url.as_deref() {
        payload.insert("applicationUrl".to_string(), json!(url));
    }
    if let Some(series) = sonarr_series(req) {
        payload.insert("series".to_string(), series);
    }

    let episodes = sonarr_episodes(req);
    if !episodes.is_empty() {
        payload.insert("episodes".to_string(), Value::Array(episodes));
    }

    match event_type {
        "Grab" => {
            if let Some(release) = sonarr_release(req) {
                payload.insert("release".to_string(), release);
            }
            insert_download_client(&mut payload, req);
            insert_custom_format_info(&mut payload, req);
        }
        "Download" => {
            let files = sonarr_episode_files(req);
            if req.event_type == NotificationEventType::ImportComplete {
                payload.insert("fileCount".to_string(), json!(files.len()));
                payload.insert("episodeFiles".to_string(), Value::Array(files));
            } else {
                if let Some(file) = files.into_iter().next() {
                    payload.insert("episodeFile".to_string(), file);
                }
                payload.insert("isUpgrade".to_string(), json!(is_upgrade(req)));
            }
            if let Some(import) = req.import.as_ref() {
                if let Some(path) = trimmed(import.source_path.as_deref()) {
                    payload.insert("sourcePath".to_string(), json!(path));
                }
                if let Some(path) = trimmed(import.dest_path.as_deref()) {
                    payload.insert("destinationPath".to_string(), json!(path));
                }
                let deleted: Vec<Value> = import
                    .deleted_paths
                    .iter()
                    .chain(import.replaced_paths.iter())
                    .map(|path| json!({ "path": path }))
                    .collect();
                if !deleted.is_empty() {
                    payload.insert("deletedFiles".to_string(), Value::Array(deleted));
                }
            }
            if let Some(release) = sonarr_grabbed_release(req) {
                payload.insert("release".to_string(), release);
            }
            insert_download_client(&mut payload, req);
            insert_custom_format_info(&mut payload, req);
        }
        "ManualInteractionRequired" => {
            payload.insert(
                "downloadStatus".to_string(),
                json!(
                    req.download
                        .as_ref()
                        .and_then(|download| download.status.clone())
                        .unwrap_or_else(|| event_label(req))
                ),
            );
            let message = trimmed(
                req.download
                    .as_ref()
                    .and_then(|download| download.status_message.as_deref()),
            )
            .map(str::to_string)
            .or_else(|| trimmed(Some(req.summary_message.as_str())).map(str::to_string));
            if let Some(message) = message {
                payload.insert(
                    "downloadStatusMessages".to_string(),
                    json!([{ "title": heading(req), "messages": [message] }]),
                );
            }
            let mut info = Map::new();
            insert_if_some(&mut info, "quality", quality(req).map(Value::from));
            info.insert("qualityVersion".to_string(), json!(1));
            insert_if_some(&mut info, "title", release_title(req).map(Value::from));
            insert_if_some(&mut info, "indexer", indexer(req).map(Value::from));
            insert_if_some(&mut info, "size", size_bytes(req).map(Value::from));
            payload.insert("downloadInfo".to_string(), Value::Object(info));
            if let Some(release) = sonarr_grabbed_release(req) {
                payload.insert("release".to_string(), release);
            }
            insert_download_client(&mut payload, req);
            insert_custom_format_info(&mut payload, req);
        }
        "Rename" => {
            let renamed: Vec<Value> = req
                .media_files
                .iter()
                .map(|file| {
                    let mut value = sonarr_episode_file(file);
                    if let Some(previous) = trimmed(file.previous_path.as_deref())
                        && let Some(object) = value.as_object_mut()
                    {
                        object.insert("previousPath".to_string(), json!(previous));
                    }
                    value
                })
                .collect();
            if !renamed.is_empty() {
                payload.insert("renamedEpisodeFiles".to_string(), Value::Array(renamed));
            }
        }
        "EpisodeFileDelete" => {
            if let Some(file) = sonarr_episode_files(req).into_iter().next() {
                payload.insert("episodeFile".to_string(), file);
            }
            // `DeleteMediaFileReason`, camelCased by the global
            // `StringEnumConverter`.
            payload.insert(
                "deleteReason".to_string(),
                json!(
                    if req.event_type == NotificationEventType::FileDeletedForUpgrade {
                        "upgrade"
                    } else {
                        "manual"
                    }
                ),
            );
        }
        "SeriesDelete" => {
            payload.insert(
                "deletedFiles".to_string(),
                json!(
                    req.import
                        .as_ref()
                        .is_some_and(|import| !import.deleted_paths.is_empty())
                        || req.summary_message.to_ascii_lowercase().contains("deleted")
                ),
            );
        }
        "Health" | "HealthRestored" => {
            let health = req.health.as_ref();
            payload.insert("level".to_string(), json!(sonarr_health_level(req)));
            payload.insert(
                "message".to_string(),
                json!(
                    health
                        .and_then(|health| trimmed(health.message.as_deref()).map(str::to_string))
                        .unwrap_or_else(|| req.summary_message.clone())
                ),
            );
            if let Some(code) = health_source(req) {
                payload.insert("type".to_string(), json!(code));
            }
        }
        "ApplicationUpdate" => {
            let update = req.application_update.as_ref();
            payload.insert(
                "message".to_string(),
                json!(
                    update
                        .and_then(|update| trimmed(update.summary.as_deref()).map(str::to_string))
                        .unwrap_or_else(|| req.summary_message.clone())
                ),
            );
            insert_if_some(
                &mut payload,
                "previousVersion",
                update
                    .and_then(|update| update.current_version.clone())
                    .map(Value::from),
            );
            insert_if_some(
                &mut payload,
                "newVersion",
                update
                    .and_then(|update| update.target_version.clone())
                    .map(Value::from),
            );
        }
        _ => {}
    }

    Ok(Value::Object(payload))
}

fn insert_if_some(target: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        target.insert(key.to_string(), value);
    }
}

fn insert_download_client(payload: &mut Map<String, Value>, req: &PluginNotificationRequest) {
    let Some(download) = req.download.as_ref() else {
        return;
    };
    insert_if_some(
        payload,
        "downloadClient",
        download.client_name.clone().map(Value::from),
    );
    insert_if_some(
        payload,
        "downloadClientType",
        download.client_type.clone().map(Value::from),
    );
    insert_if_some(
        payload,
        "downloadId",
        download.download_id.clone().map(Value::from),
    );
}

fn insert_custom_format_info(payload: &mut Map<String, Value>, req: &PluginNotificationRequest) {
    let Some(scores) = release(req).map(|release| &release.custom_scores) else {
        return;
    };
    if scores.is_empty() {
        return;
    }
    payload.insert(
        "customFormatInfo".to_string(),
        json!({
            // Sonarr's `WebhookCustomFormat` carries an int id; Scryer's
            // contract carries names and scores only, so the id is 0 and the
            // name is the whole truth.
            "customFormats": scores
                .keys()
                .map(|name| json!({ "id": 0, "name": name }))
                .collect::<Vec<_>>(),
            "customFormatScore": scores.values().sum::<i32>(),
        }),
    );
}

/// `SeriesTypes`, camelCased: `standard` / `daily` / `anime`.
fn sonarr_series_type(req: &PluginNotificationRequest) -> &'static str {
    match facet(req).as_deref() {
        Some("anime") => "anime",
        _ => "standard",
    }
}

fn sonarr_series(req: &PluginNotificationRequest) -> Option<Value> {
    let title = req.title.as_ref()?;
    let ids = &title.external_ids;
    let mut series = Map::new();
    series.insert(
        "id".to_string(),
        json!(numeric_id(title.id.as_deref()).unwrap_or(0)),
    );
    series.insert("title".to_string(), json!(title.name));
    insert_if_some(
        &mut series,
        "titleSlug",
        trimmed(title.slug.as_deref()).map(Value::from),
    );
    insert_if_some(
        &mut series,
        "path",
        trimmed(title.path.as_deref()).map(Value::from),
    );
    if let Some(id) = external_id(ids.tvdb_id.as_deref(), ids, "tvdb")
        .as_deref()
        .and_then(numeric)
    {
        series.insert("tvdbId".to_string(), json!(id));
    }
    if let Some(id) = external_id(ids.tvmaze_id.as_deref(), ids, "tvmaze")
        .as_deref()
        .and_then(numeric)
    {
        series.insert("tvMazeId".to_string(), json!(id));
    }
    if let Some(id) = external_id(ids.tmdb_id.as_deref(), ids, "tmdb")
        .as_deref()
        .and_then(numeric)
    {
        series.insert("tmdbId".to_string(), json!(id));
    }
    insert_if_some(
        &mut series,
        "imdbId",
        external_id(ids.imdb_id.as_deref(), ids, "imdb").map(Value::from),
    );
    let mal: Vec<i64> = ids
        .mal_ids
        .iter()
        .filter_map(|id| numeric(id.as_str()))
        .collect();
    if !mal.is_empty() {
        series.insert("malIds".to_string(), json!(mal));
    }
    let anilist: Vec<i64> = ids
        .anilist_ids
        .iter()
        .filter_map(|id| numeric(id.as_str()))
        .collect();
    if !anilist.is_empty() {
        series.insert("aniListIds".to_string(), json!(anilist));
    }
    series.insert("type".to_string(), json!(sonarr_series_type(req)));
    if let Some(year) = title.year {
        series.insert("year".to_string(), json!(year));
    }
    let mut images = Vec::new();
    if let Some(poster) = trimmed(title.poster_url.as_deref()) {
        images.push(json!({ "coverType": "poster", "url": poster, "remoteUrl": poster }));
    }
    if let Some(fanart) = trimmed(title.background_url.as_deref()) {
        images.push(json!({ "coverType": "fanart", "url": fanart, "remoteUrl": fanart }));
    }
    if !images.is_empty() {
        series.insert("images".to_string(), Value::Array(images));
    }
    if !title.tags.is_empty() {
        series.insert("tags".to_string(), json!(title.tags));
    }
    insert_if_some(
        &mut series,
        "originalCountry",
        trimmed(title.original_country.as_deref()).map(Value::from),
    );
    Some(Value::Object(series))
}

fn sonarr_episodes(req: &PluginNotificationRequest) -> Vec<Value> {
    episode_list(req)
        .into_iter()
        .map(|episode| {
            let mut value = Map::new();
            value.insert(
                "id".to_string(),
                json!(numeric_id(episode.id.as_deref()).unwrap_or(0)),
            );
            if let Some(number) = trimmed(episode.episode_number.as_deref()).and_then(numeric) {
                value.insert("episodeNumber".to_string(), json!(number));
            }
            if let Some(number) = trimmed(episode.season_number.as_deref()).and_then(numeric) {
                value.insert("seasonNumber".to_string(), json!(number));
            }
            insert_if_some(
                &mut value,
                "title",
                trimmed(episode.title.as_deref()).map(Value::from),
            );
            insert_if_some(
                &mut value,
                "overview",
                trimmed(episode.overview.as_deref()).map(Value::from),
            );
            insert_if_some(
                &mut value,
                "airDate",
                trimmed(episode.air_date.as_deref()).map(Value::from),
            );
            insert_if_some(
                &mut value,
                "airDateUtc",
                trimmed(episode.air_date_utc.as_deref()).map(Value::from),
            );
            insert_if_some(
                &mut value,
                "finaleType",
                trimmed(episode.finale_type.as_deref()).map(Value::from),
            );
            if let Some(id) = trimmed(episode.tvdb_id.as_deref()).and_then(numeric) {
                value.insert("tvdbId".to_string(), json!(id));
            }
            Value::Object(value)
        })
        .collect()
}

/// `WebhookRelease` — the grab shape, which carries quality and custom formats.
fn sonarr_release(req: &PluginNotificationRequest) -> Option<Value> {
    let release = release(req)?;
    let mut value = Map::new();
    insert_if_some(&mut value, "quality", quality(req).map(Value::from));
    value.insert("qualityVersion".to_string(), json!(1));
    insert_if_some(
        &mut value,
        "releaseGroup",
        release_group(req).map(Value::from),
    );
    insert_if_some(
        &mut value,
        "releaseTitle",
        release_title(req).map(Value::from),
    );
    insert_if_some(&mut value, "indexer", indexer(req).map(Value::from));
    if let Some(size) = size_bytes(req) {
        value.insert("size".to_string(), json!(size));
    }
    if !release.custom_scores.is_empty() {
        value.insert(
            "customFormats".to_string(),
            json!(release.custom_scores.keys().collect::<Vec<_>>()),
        );
        value.insert(
            "customFormatScore".to_string(),
            json!(release.custom_scores.values().sum::<i32>()),
        );
    }
    if !release.languages.is_empty() {
        value.insert(
            "languages".to_string(),
            json!(
                release
                    .languages
                    .iter()
                    .map(|name| json!({ "id": 0, "name": name }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    Some(Value::Object(value))
}

/// `WebhookGrabbedRelease` — the import shape, which carries only the release's
/// own identity.
fn sonarr_grabbed_release(req: &PluginNotificationRequest) -> Option<Value> {
    let title = release_title(req);
    let indexer = indexer(req);
    let size = size_bytes(req);
    if title.is_none() && indexer.is_none() && size.is_none() {
        return None;
    }
    let mut value = Map::new();
    insert_if_some(&mut value, "releaseTitle", title.map(Value::from));
    insert_if_some(&mut value, "indexer", indexer.map(Value::from));
    insert_if_some(&mut value, "size", size.map(Value::from));
    Some(Value::Object(value))
}

fn sonarr_episode_files(req: &PluginNotificationRequest) -> Vec<Value> {
    req.media_files.iter().map(sonarr_episode_file).collect()
}

fn sonarr_episode_file(file: &PluginNotificationMediaFile) -> Value {
    let mut value = Map::new();
    value.insert(
        "id".to_string(),
        json!(numeric_id(file.id.as_deref()).unwrap_or(0)),
    );
    value.insert("path".to_string(), json!(file.path));
    insert_if_some(
        &mut value,
        "quality",
        trimmed(file.quality.as_deref()).map(Value::from),
    );
    value.insert("qualityVersion".to_string(), json!(1));
    insert_if_some(
        &mut value,
        "releaseGroup",
        trimmed(file.release_group.as_deref()).map(Value::from),
    );
    insert_if_some(
        &mut value,
        "sceneName",
        trimmed(file.scene_name.as_deref()).map(Value::from),
    );
    if let Some(size) = file.size_bytes {
        value.insert("size".to_string(), json!(size));
    }
    insert_if_some(
        &mut value,
        "recycleBinPath",
        trimmed(file.recycle_bin_path.as_deref()).map(Value::from),
    );

    let mut media_info = Map::new();
    insert_if_some(
        &mut media_info,
        "audioCodec",
        trimmed(file.audio_codec.as_deref()).map(Value::from),
    );
    if let Some(channels) = trimmed(file.audio_channels.as_deref())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
    {
        media_info.insert("audioChannels".to_string(), json!(channels));
    }
    if !file.audio_languages.is_empty() {
        media_info.insert("audioLanguages".to_string(), json!(file.audio_languages));
    }
    if !file.subtitle_languages.is_empty() {
        media_info.insert("subtitles".to_string(), json!(file.subtitle_languages));
    }
    insert_if_some(
        &mut media_info,
        "videoCodec",
        trimmed(file.video_codec.as_deref()).map(Value::from),
    );
    insert_if_some(
        &mut media_info,
        "videoDynamicRangeType",
        trimmed(file.video_hdr_format.as_deref()).map(Value::from),
    );
    if let Some(width) = file.video_width {
        media_info.insert("width".to_string(), json!(width));
    }
    if let Some(height) = file.video_height {
        media_info.insert("height".to_string(), json!(height));
    }
    if !media_info.is_empty() {
        value.insert("mediaInfo".to_string(), Value::Object(media_info));
    }

    Value::Object(value)
}

/// `HealthCheckResult`, camelCased by the global `StringEnumConverter`.
fn sonarr_health_level(req: &PluginNotificationRequest) -> &'static str {
    match req.severity {
        Some(NotificationSeverity::Error) => "error",
        Some(NotificationSeverity::Warning) => "warning",
        _ if req.event_type == NotificationEventType::HealthRestored => "ok",
        _ => "warning",
    }
}

/// Sonarr's ids are 32-bit database keys; Scryer's are opaque strings. A
/// non-numeric id becomes `0`, which is what Sonarr sends for an unsaved entity
/// and what Notifiarr therefore already tolerates.
fn numeric_id(id: Option<&str>) -> Option<i64> {
    trimmed(id).and_then(numeric)
}

fn numeric(value: &str) -> Option<i64> {
    value
        .trim()
        .trim_start_matches("tt")
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn clamp(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.to_string(), false);
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    (out, true)
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn clamp_member(
    parent: &mut Value,
    key: &str,
    limit: usize,
    label: &str,
    warnings: &mut Vec<String>,
) {
    let Some(value) = parent.get_mut(key) else {
        return;
    };
    let Some(text) = value.as_str() else { return };
    let (clamped, truncated) = clamp(text, limit);
    if truncated {
        warnings.push(format!(
            "{label} truncated to Discord's {limit}-character limit"
        ));
        *value = json!(clamped);
    }
}

fn embed_character_total(text: &Value) -> usize {
    let mut total = 0;
    for key in ["title", "description", "footer", "content"] {
        total += text.get(key).and_then(Value::as_str).map_or(0, char_count);
    }
    if let Some(fields) = text.get("fields").and_then(Value::as_array) {
        for field in fields {
            for key in ["title", "text"] {
                total += field.get(key).and_then(Value::as_str).map_or(0, char_count);
            }
        }
    }
    total
}

/// Notifiarr hands the payload straight to Discord, which rejects an over-limit
/// embed outright. Trim to fit and tell the core what was lost.
fn enforce_embed_limits(payload: &mut Value, warnings: &mut Vec<String>) {
    let Some(text) = payload.pointer_mut("/discord/text") else {
        return;
    };
    clamp_member(text, "title", EMBED_TITLE_LIMIT, "title", warnings);
    clamp_member(
        text,
        "description",
        EMBED_DESCRIPTION_LIMIT,
        "description",
        warnings,
    );
    clamp_member(text, "content", EMBED_CONTENT_LIMIT, "content", warnings);
    clamp_member(text, "footer", EMBED_FOOTER_LIMIT, "footer", warnings);

    if let Some(fields) = text.get_mut("fields").and_then(Value::as_array_mut) {
        if fields.len() > EMBED_FIELD_COUNT_LIMIT {
            warnings.push(format!(
                "dropped {} field(s) over Notifiarr's {EMBED_FIELD_COUNT_LIMIT}-field limit",
                fields.len() - EMBED_FIELD_COUNT_LIMIT
            ));
            fields.truncate(EMBED_FIELD_COUNT_LIMIT);
        }
        for field in fields.iter_mut() {
            clamp_member(
                field,
                "title",
                EMBED_FIELD_NAME_LIMIT,
                "field title",
                warnings,
            );
            clamp_member(
                field,
                "text",
                EMBED_FIELD_VALUE_LIMIT,
                "field text",
                warnings,
            );
        }
    }

    let mut dropped = 0usize;
    while embed_character_total(text) > EMBED_TOTAL_LIMIT {
        let Some(fields) = text.get_mut("fields").and_then(Value::as_array_mut) else {
            break;
        };
        if fields.pop().is_none() {
            break;
        }
        dropped += 1;
    }
    if dropped > 0 {
        warnings.push(format!(
            "dropped {dropped} field(s) to stay under Discord's {EMBED_TOTAL_LIMIT}-character embed limit"
        ));
    }

    let total = embed_character_total(text);
    if total > EMBED_TOTAL_LIMIT
        && let Some(description) = text.get("description").and_then(Value::as_str)
    {
        let budget = char_count(description).saturating_sub(total - EMBED_TOTAL_LIMIT);
        let (clamped, _) = clamp(description, budget);
        warnings.push(format!(
            "description truncated to stay under Discord's {EMBED_TOTAL_LIMIT}-character embed limit"
        ));
        text["description"] = json!(clamped);
    }
}

/// `Discord.BytesToString` (`Discord.cs:676-688`).
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
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let (settings, mut warnings) = match Settings::from_config(req.is_test) {
        Ok(resolved) => resolved,
        Err(error) => return PluginResult::Err(error),
    };

    let payload = match settings.integration {
        Integration::Passthrough => build_passthrough_payload(req, &settings, &mut warnings),
        Integration::Sonarr => match build_sonarr_payload(req, &settings, &mut warnings) {
            Ok(payload) => payload,
            Err(error) => return PluginResult::Err(error),
        },
    };

    // A Test-time-only probe against Notifiarr's own key-validation route. It
    // answers the one question the notification endpoints cannot separate from a
    // disabled integration: is this API key accepted at all? Everything it finds
    // is a warning; the post immediately afterwards produces the real error.
    if req.is_test {
        warnings.extend(probe_api_key(&settings.api_key));
    }

    let body = match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(error) => {
            return PluginResult::Err(plugin_error(
                PluginErrorCode::Permanent,
                "could not encode the Notifiarr payload".to_string(),
                Some(error.to_string()),
            ));
        }
    };

    // The API key travels in the path for the passthrough integration (the shape
    // the wiki documents and the shape Shoutrrr and Apprise use) and in
    // `X-API-Key` for both, which is what Notifiarr's own client sends. The URL
    // is therefore a secret and never appears in a message this plugin returns.
    let url = match settings.integration {
        Integration::Passthrough => format!(
            "{BASE_URL}{PASSTHROUGH_PATH}/{}",
            path_segment(&settings.api_key)
        ),
        Integration::Sonarr => format!("{BASE_URL}{SONARR_PATH}"),
    };

    let request = HttpRequest::new(&url)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_header("X-API-Key", &settings.api_key)
        .with_header("User-Agent", USER_AGENT);

    match http::request::<Vec<u8>>(&request, Some(body)) {
        Ok(response) => classify_response(
            response.status_code(),
            response.headers(),
            &response.body(),
            &settings,
            warnings,
        ),
        Err(error) => {
            // The host answers a refused or failed egress in-band; that is a
            // delivery failure, not a channel misconfiguration.
            let mut failure = error_response(format!("request failed: {error}"), None);
            failure.warnings = warnings;
            failure.target_results = vec![target_result(&settings, false, None, None)];
            PluginResult::Ok(failure)
        }
    }
}

/// `GET /api/v1/user/validate?event=user` with `X-API-Key`
/// (`website.go:334-368`). Warnings only: a probe that cannot reach Notifiarr
/// must not stop the notification that follows it.
fn probe_api_key(api_key: &str) -> Vec<String> {
    let request = HttpRequest::new(format!("{BASE_URL}{VALIDATE_PATH}"))
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("X-API-Key", api_key)
        .with_header("User-Agent", USER_AGENT);

    match http::request::<Vec<u8>>(&request, None) {
        Ok(response) => match response.status_code() {
            200..=299 => Vec::new(),
            401 | 403 => vec![
                "Notifiarr rejected api_key on its own validation endpoint; check the key on notifiarr.com under Profile".to_string(),
            ],
            status => vec![format!(
                "Notifiarr's API-key validation endpoint answered HTTP {status}; the key could not be confirmed"
            )],
        },
        Err(error) => vec![format!(
            "could not reach Notifiarr's API-key validation endpoint: {error}"
        )],
    }
}

/// Notifiarr's answer, whatever shape it arrived in.
#[derive(Debug, Default)]
struct NotifiarrBody {
    is_json: bool,
    /// The `result` member: `"success"` on a delivered notification.
    result: Option<String>,
    /// `details.response`, `details.help`, `message` or `error`, whichever the
    /// answer carried.
    detail: Option<String>,
}

impl NotifiarrBody {
    fn succeeded(&self) -> bool {
        match self.result.as_deref() {
            // An answer that says nothing about the result — including the empty
            // `{}` a proxy may substitute — is taken at its status code.
            None => true,
            Some(result) => result.eq_ignore_ascii_case("success"),
        }
    }

    fn detail(&self, status: u16) -> String {
        self.detail
            .clone()
            .unwrap_or_else(|| format!("HTTP {status} with no message from Notifiarr"))
    }
}

/// `{"result": "...", "details": {"response": ..., "help": ...}}`
/// (`pkg/website` `Response`, and the wiki's `response.result === 'success'`).
/// Older and proxied answers use `message`/`error`, so both are read.
fn parse_notifiarr_body(body: &[u8]) -> NotifiarrBody {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let non_json = || NotifiarrBody {
        is_json: false,
        result: None,
        detail: (!text.is_empty()).then(|| clamp(text, 500).0),
    };

    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return non_json();
    };
    if !value.is_object() {
        return non_json();
    }

    let result = ["result", "response", "status"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_string);

    let detail = value
        .pointer("/details/response")
        .or_else(|| value.pointer("/details/help"))
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error"))
        .or_else(|| value.get("details"))
        .map(render_detail)
        .filter(|detail| !detail.is_empty());

    NotifiarrBody {
        is_json: true,
        result,
        detail,
    }
}

fn render_detail(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Array(items) => items
            .iter()
            .map(render_detail)
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    settings: &Settings,
    warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let answer = parse_notifiarr_body(body);
    let detail = answer.detail(status);
    let debug = format!("HTTP {status}: {detail}");
    let retry_after = retry_after_seconds(headers);

    if (200..300).contains(&status) {
        if answer.succeeded() {
            let mut response = ok_response();
            response.provider_status = Some(format!("http_{status}"));
            response.warnings = warnings;
            response.target_results = vec![target_result(
                settings,
                true,
                Some(format!("http_{status}")),
                None,
            )];
            return PluginResult::Ok(response);
        }
        // Notifiarr answers `200` with `{"result":"error", …}` when the
        // integration itself refused the payload. Sonarr never looks at the
        // body, so it reports these as delivered.
        let mut failure = error_response(
            format!("Notifiarr accepted the request but did not deliver it: {detail}"),
            Some("notifiarr_error".to_string()),
        );
        failure.warnings = warnings;
        failure.target_results = vec![target_result(
            settings,
            false,
            Some("notifiarr_error".to_string()),
            Some(detail),
        )];
        return PluginResult::Ok(failure);
    }

    // Notifiarr is fronted by Cloudflare, which answers with an HTML page and
    // borrows Notifiarr's status codes — including `400`
    // (`Notifiarr/notifiarr:pkg/website/website.go:53-60`). Blaming `api_key`
    // for that would send the operator to the wrong setting.
    if !answer.is_json && !(500..600).contains(&status) && status != 429 && status != 401 {
        let mut failure = error_response(
            format!(
                "notifiarr.com answered HTTP {status} with a non-JSON body, so the answer came from something in front of Notifiarr (usually Cloudflare) rather than from the API: {detail}"
            ),
            Some(format!("http_{status}")),
        );
        failure.retry_after_seconds = retry_after;
        failure.warnings = warnings;
        failure.target_results = vec![target_result(
            settings,
            false,
            Some(format!("http_{status}")),
            Some(detail),
        )];
        return PluginResult::Ok(failure);
    }

    match status {
        // `NotifiarrProxy.cs:178-180`. Notifiarr's own client treats 401 and 403
        // alike (`website.go:316`).
        401 | 403 => PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!("api_key was rejected by Notifiarr (HTTP {status}): {detail}"),
            Some(debug),
        )),
        // Sonarr logs a 400 and reports success, on the grounds that one
        // misconfigured event should not stop the others
        // (`NotifiarrProxy.cs:181-185`). That leniency is what made every
        // delivery from the June port look like a success. Scryer reports it.
        400 => {
            let hint = match settings.integration {
                Integration::Passthrough => {
                    "Enable the Passthrough integration on notifiarr.com and check that channel_id names a channel it may post to."
                }
                Integration::Sonarr => {
                    "Enable the Sonarr integration on notifiarr.com and assign it a channel."
                }
            };
            let mut failure = error_response(
                format!("Notifiarr rejected the notification (HTTP 400): {detail}. {hint}"),
                Some("http_400".to_string()),
            );
            failure.warnings = warnings;
            failure.target_results = vec![target_result(
                settings,
                false,
                Some("http_400".to_string()),
                Some(detail),
            )];
            PluginResult::Ok(failure)
        }
        // Every notification route this plugin uses has existed for the life of
        // the API, so a 404 is the integration name, not a bad moment.
        404 => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "Notifiarr has no '{}' integration endpoint (HTTP 404): {detail}",
                settings.integration.label()
            ),
            Some(debug),
        )),
        // Notifiarr rate-limits per account tier — 12,000/day and 500/hour for a
        // free account, 24,000/day and 1,000/hour for a patron
        // (notifiarr.wiki/pages/faq/faq/). Sonarr has no 429 arm at all.
        429 => {
            let mut failure = error_response(
                format!(
                    "Notifiarr rate-limited this channel (HTTP 429): {detail}. Free accounts allow 500 notifications an hour."
                ),
                Some("http_429".to_string()),
            );
            failure.retry_after_seconds = retry_after;
            failure.warnings = warnings;
            failure.target_results = vec![target_result(
                settings,
                false,
                Some("http_429".to_string()),
                Some(detail),
            )];
            PluginResult::Ok(failure)
        }
        // `NotifiarrProxy.cs:186-196`: 502/503/504 are Notifiarr being down,
        // 520-524 are Cloudflare's own five-hundreds. Both are the provider
        // saying "not now" — the delivery lane.
        _ => {
            let reason = match status {
                502..=504 => "Notifiarr is unavailable",
                520..=524 => "Cloudflare could not reach Notifiarr",
                _ => "Notifiarr returned an unexpected status",
            };
            let mut failure = error_response(
                format!("{reason} (HTTP {status}): {detail}"),
                Some(format!("http_{status}")),
            );
            failure.retry_after_seconds = retry_after;
            failure.warnings = warnings;
            failure.target_results = vec![target_result(
                settings,
                false,
                Some(format!("http_{status}")),
                Some(detail),
            )];
            PluginResult::Ok(failure)
        }
    }
}

fn retry_after_seconds(headers: &BTreeMap<String, String>) -> Option<i64> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<i64>().ok())
        .filter(|seconds| *seconds >= 0)
        .map(|seconds| seconds.max(1))
}

/// The core never reads `target_results` today, but the contract carries it and
/// this channel does have a nameable target: the integration, plus the channel
/// the passthrough posts to.
fn target_result(
    settings: &Settings,
    success: bool,
    status: Option<String>,
    error: Option<String>,
) -> PluginNotificationTargetResult {
    let target = match (settings.integration, settings.channel_id) {
        (Integration::Passthrough, Some(channel)) => format!("notifiarr:passthrough/{channel}"),
        (integration, _) => format!("notifiarr:{}", integration.label()),
    };
    PluginNotificationTargetResult {
        target,
        success,
        status,
        error,
    }
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
        NotificationMediaUpdateType, PluginNotificationApp, PluginNotificationApplicationUpdate,
        PluginNotificationDownload, PluginNotificationExternalIds, PluginNotificationFile,
        PluginNotificationHealth, PluginNotificationImport, PluginNotificationMediaUpdate,
    };

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// The sparse shape the core actually sends today.
    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: Some("evt-1".to_string()),
            occurred_at: Some("2026-09-01T10:00:00Z".to_string()),
            correlation_id: None,
            actor: None,
            severity: None,
            is_test: event_type == NotificationEventType::Test,
            summary_title: "Summary".to_string(),
            summary_message: "Summary message.".to_string(),
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
            id: Some("41".to_string()),
            name: "Cinder Line".to_string(),
            facet: "series".to_string(),
            year: Some(2019),
            slug: Some("cinder-line".to_string()),
            path: Some("/media/TV/Cinder Line".to_string()),
            overview: Some("A show about a line.".to_string()),
            sort_title: None,
            background_url: Some("https://images.test/fanart.jpg".to_string()),
            poster_url: Some("https://images.test/poster.jpg".to_string()),
            tags: vec!["4k".to_string()],
            aliases: Vec::new(),
            original_language: Some("English".to_string()),
            original_country: Some("US".to_string()),
            external_ids: PluginNotificationExternalIds {
                tvdb_id: Some("81189".to_string()),
                imdb_id: Some("tt0903747".to_string()),
                tvmaze_id: Some("169".to_string()),
                ..PluginNotificationExternalIds::default()
            },
        }
    }

    /// Every optional block populated: the shape the contract *can* carry.
    fn full_request(event_type: NotificationEventType) -> PluginNotificationRequest {
        let mut req = request(event_type);
        req.severity = Some(NotificationSeverity::Info);
        req.title = Some(series_title());
        req.episode = Some(PluginNotificationEpisode {
            id: Some("900".to_string()),
            episode_ids: vec!["900".to_string()],
            season_number: Some("2".to_string()),
            episode_number: Some("5".to_string()),
            title: Some("Ember".to_string()),
            overview: Some("An episode about embers.".to_string()),
            air_date: Some("2019-04-01".to_string()),
            air_date_utc: Some("2019-04-01T01:00:00Z".to_string()),
            tvdb_id: Some("7788".to_string()),
            ..PluginNotificationEpisode::default()
        });
        req.episodes = vec![req.episode.clone().unwrap()];
        req.release = Some(PluginNotificationRelease {
            source_title: Some("Cinder.Line.S02E05.1080p.WEB-DL-GRP".to_string()),
            quality: Some("WEBDL-1080p".to_string()),
            release_group: Some("GRP".to_string()),
            indexer: Some("Test Indexer".to_string()),
            languages: vec!["English".to_string()],
            custom_scores: BTreeMap::from([("Surround".to_string(), 25)]),
            ..PluginNotificationRelease::default()
        });
        req.download = Some(PluginNotificationDownload {
            download_id: Some("abc123".to_string()),
            client_name: Some("Weaver".to_string()),
            client_type: Some("usenet".to_string()),
            title: Some("Cinder.Line.S02E05".to_string()),
            size_bytes: Some(3_221_225_472),
            ..PluginNotificationDownload::default()
        });
        req.import = Some(PluginNotificationImport {
            source_path: Some("/downloads/Cinder.Line.S02E05".to_string()),
            dest_path: Some("/media/TV/Cinder Line/S02/E05.mkv".to_string()),
            imported_count: Some(1),
            upgrade: false,
            ..PluginNotificationImport::default()
        });
        req.media_files = vec![PluginNotificationMediaFile {
            id: Some("5150".to_string()),
            path: "/media/TV/Cinder Line/S02/E05.mkv".to_string(),
            size_bytes: Some(3_221_225_472),
            quality: Some("WEBDL-1080p".to_string()),
            release_group: Some("GRP".to_string()),
            scene_name: Some("Cinder.Line.S02E05.1080p.WEB-DL-GRP".to_string()),
            audio_languages: vec!["English".to_string()],
            subtitle_languages: vec!["English".to_string(), "French".to_string()],
            video_codec: Some("x264".to_string()),
            audio_codec: Some("EAC3".to_string()),
            audio_channels: Some("5.1".to_string()),
            video_width: Some(1920),
            video_height: Some(1080),
            ..PluginNotificationMediaFile::default()
        }];
        req
    }

    fn text_of(payload: &Value) -> &Value {
        payload.pointer("/discord/text").expect("discord.text")
    }

    fn field_titles(payload: &Value) -> Vec<String> {
        text_of(payload)
            .get("fields")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|field| field.get("title").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn field_text(payload: &Value, title: &str) -> Option<String> {
        text_of(payload)
            .get("fields")?
            .as_array()?
            .iter()
            .find(|field| field.get("title").and_then(Value::as_str) == Some(title))?
            .get("text")?
            .as_str()
            .map(str::to_string)
    }

    fn passthrough(req: &PluginNotificationRequest) -> (Value, Vec<String>) {
        let mut warnings = Vec::new();
        let payload = build_passthrough_payload(req, &Settings::passthrough(), &mut warnings);
        (payload, warnings)
    }

    fn sonarr(req: &PluginNotificationRequest) -> (Value, Vec<String>) {
        let mut warnings = Vec::new();
        let payload = build_sonarr_payload(req, &Settings::sonarr(), &mut warnings)
            .expect("the event has a Sonarr equivalent");
        (payload, warnings)
    }

    // -----------------------------------------------------------------------
    // Passthrough payload
    // -----------------------------------------------------------------------

    /// The whole point of the reconciliation: what leaves this plugin must be
    /// the schema Notifiarr's passthrough integration parses, not Scryer's own
    /// request JSON.
    #[test]
    fn passthrough_payload_matches_notifiarrs_documented_schema() {
        let (payload, warnings) = passthrough(&full_request(NotificationEventType::Grab));

        assert_eq!(payload["notification"]["name"], json!("Scryer"));
        assert_eq!(payload["notification"]["update"], json!(false));
        assert_eq!(payload["notification"]["event"], json!("evt-1"));
        assert_eq!(
            payload["discord"]["ids"]["channel"],
            json!(910_000_000_000_000_001i64)
        );
        assert_eq!(payload["discord"]["color"], json!("FFC230"));
        assert_eq!(
            text_of(&payload)["title"],
            json!("Cinder Line - 2x05 - Ember")
        );
        assert_eq!(
            payload["discord"]["images"]["thumbnail"],
            json!("https://images.test/poster.jpg")
        );
        assert_eq!(
            payload["discord"]["images"]["image"],
            json!("https://images.test/fanart.jpg")
        );
        // No trace of Scryer's own request shape.
        assert!(payload.get("event_type").is_none());
        assert!(payload.get("summary_title").is_none());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// The passthrough field object is `{title, text, inline}`, not Discord's
    /// `{name, value, inline}`.
    #[test]
    fn passthrough_fields_use_notifiarrs_member_names() {
        let (payload, _) = passthrough(&full_request(NotificationEventType::Grab));
        let fields = text_of(&payload)["fields"].as_array().unwrap().clone();
        assert!(!fields.is_empty());
        for field in fields {
            assert!(field.get("title").is_some(), "{field}");
            assert!(field.get("text").is_some(), "{field}");
            assert!(field.get("name").is_none(), "{field}");
            assert!(field.get("value").is_none(), "{field}");
        }
    }

    #[test]
    fn passthrough_renders_every_populated_block() {
        let (payload, _) = passthrough(&full_request(NotificationEventType::ImportComplete));
        assert_eq!(
            field_text(&payload, "Quality").as_deref(),
            Some("WEBDL-1080p")
        );
        assert_eq!(field_text(&payload, "Group").as_deref(), Some("GRP"));
        assert_eq!(field_text(&payload, "Size").as_deref(), Some("3 GB"));
        assert_eq!(
            field_text(&payload, "Codecs").as_deref(),
            Some("x264 / EAC3 5.1")
        );
        assert_eq!(
            field_text(&payload, "Languages").as_deref(),
            Some("English")
        );
        assert_eq!(
            field_text(&payload, "Subtitles").as_deref(),
            Some("English/French")
        );
        assert_eq!(
            field_text(&payload, "Indexer").as_deref(),
            Some("Test Indexer")
        );
        assert_eq!(
            field_text(&payload, "Download Client").as_deref(),
            Some("Weaver")
        );
        assert_eq!(
            field_text(&payload, "Custom Formats").as_deref(),
            Some("Surround")
        );
        assert_eq!(
            field_text(&payload, "Custom Format Score").as_deref(),
            Some("25")
        );
        assert_eq!(
            field_text(&payload, "Destination").as_deref(),
            Some("/media/TV/Cinder Line/S02/E05.mkv")
        );
        let links = field_text(&payload, "Links").expect("links");
        assert!(
            links.contains("thetvdb.com/?tab=series&id=81189"),
            "{links}"
        );
        assert!(links.contains("tvmaze.com/shows/169"), "{links}");
        assert!(links.contains("imdb.com/title/tt0903747/"), "{links}");
    }

    /// The sparse shape the core sends today must still render, and must not
    /// invent fields it has no data for.
    #[test]
    fn passthrough_renders_the_sparse_shape_the_core_sends() {
        let (payload, warnings) = passthrough(&request(NotificationEventType::TitleAdded));
        assert_eq!(text_of(&payload)["title"], json!("Summary"));
        assert_eq!(
            text_of(&payload)["description"],
            json!("Added\nSummary message.")
        );
        assert!(field_titles(&payload).is_empty(), "{payload}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Common rule 2's spirit: an event this channel has no special case for
    /// renders generically instead of failing.
    #[test]
    fn a_scryer_only_event_renders_generically_on_passthrough() {
        let (payload, _) = passthrough(&full_request(NotificationEventType::SubtitleDownloaded));
        assert_eq!(payload["discord"]["color"], json!("27C24C"));
        assert!(
            text_of(&payload)["description"]
                .as_str()
                .unwrap()
                .starts_with("Subtitle Downloaded")
        );
        assert!(field_titles(&payload).contains(&"Quality".to_string()));
    }

    #[test]
    fn a_movie_facet_uses_movie_links_and_neutral_wording() {
        let mut req = full_request(NotificationEventType::Grab);
        let title = req.title.as_mut().unwrap();
        title.facet = "movie".to_string();
        title.external_ids.tmdb_id = Some("603".to_string());
        title.external_ids.tvdb_id = None;
        req.episode = None;
        req.episodes = Vec::new();

        let (payload, warnings) = passthrough(&req);
        assert_eq!(text_of(&payload)["title"], json!("Cinder Line"));
        assert!(
            text_of(&payload)["description"]
                .as_str()
                .unwrap()
                .starts_with("Grabbed")
        );
        let links = field_text(&payload, "Links").expect("links");
        assert!(links.contains("themoviedb.org/movie/603"), "{links}");
        assert!(
            warnings.is_empty(),
            "the passthrough tells the truth about facets: {warnings:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Colours and severity
    // -----------------------------------------------------------------------

    /// `NotificationEventType::Download` is a FAILED download in Scryer, and the
    /// dispatcher stamps `severity: Error` on it. Rendering it green would be
    /// the worst possible lie.
    #[test]
    fn a_failed_download_is_red_and_labelled_as_a_failure() {
        let mut req = full_request(NotificationEventType::Download);
        req.severity = Some(NotificationSeverity::Error);
        req.summary_title = "Download failed: Cinder Line".to_string();

        let (payload, _) = passthrough(&req);
        assert_eq!(payload["discord"]["color"], json!("F05050"));
        assert!(
            text_of(&payload)["description"]
                .as_str()
                .unwrap()
                .starts_with("Download Failed")
        );
    }

    #[test]
    fn colour_table_follows_sonarrs_discord_colours() {
        let cases = [
            (NotificationEventType::Grab, "FFC230"),
            (NotificationEventType::Upgrade, "3E6800"),
            (NotificationEventType::ImportComplete, "27C24C"),
            (NotificationEventType::TitleDeleted, "F05050"),
            (NotificationEventType::HealthIssue, "FFA500"),
            (NotificationEventType::Test, "FFC230"),
        ];
        for (event_type, expected) in cases {
            let (payload, _) = passthrough(&request(event_type));
            assert_eq!(
                payload["discord"]["color"],
                json!(expected),
                "{event_type:?}"
            );
        }
    }

    #[test]
    fn severity_overrides_the_event_colour_but_never_downgrades_red() {
        let mut warning = request(NotificationEventType::Grab);
        warning.severity = Some(NotificationSeverity::Warning);
        assert_eq!(color(&warning), COLOR_WARNING);

        let mut warned_delete = request(NotificationEventType::TitleDeleted);
        warned_delete.severity = Some(NotificationSeverity::Warning);
        assert_eq!(color(&warned_delete), COLOR_DANGER);
    }

    // -----------------------------------------------------------------------
    // Event-specific rendering
    // -----------------------------------------------------------------------

    #[test]
    fn a_deleted_file_renders_the_reason_and_the_deleted_path() {
        let mut req = full_request(NotificationEventType::FileDeletedForUpgrade);
        req.file = Some(PluginNotificationFile {
            primary_path: None,
            media_updates: vec![
                PluginNotificationMediaUpdate {
                    path: "/media/TV/Cinder Line/S02/E05.old.mkv".to_string(),
                    update_type: NotificationMediaUpdateType::Deleted,
                },
                PluginNotificationMediaUpdate {
                    path: "/media/TV/Cinder Line/S02/E05.mkv".to_string(),
                    update_type: NotificationMediaUpdateType::Created,
                },
            ],
        });

        let (payload, _) = passthrough(&req);
        assert_eq!(
            field_text(&payload, "Reason").as_deref(),
            Some("Episode Deleted for Upgrade")
        );
        assert_eq!(
            field_text(&payload, "File").as_deref(),
            Some("/media/TV/Cinder Line/S02/E05.old.mkv")
        );
    }

    #[test]
    fn an_application_update_renders_both_versions() {
        let mut req = request(NotificationEventType::ApplicationUpdate);
        req.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            summary: Some("Scryer updated.".to_string()),
            ..PluginNotificationApplicationUpdate::default()
        });

        let (payload, _) = passthrough(&req);
        assert_eq!(text_of(&payload)["description"], json!("Scryer updated."));
        assert_eq!(
            field_text(&payload, "Previous Version").as_deref(),
            Some("0.19.7")
        );
        assert_eq!(
            field_text(&payload, "New Version").as_deref(),
            Some("0.19.8")
        );
    }

    #[test]
    fn a_health_issue_uses_the_health_block_for_its_heading_and_body() {
        let mut req = request(NotificationEventType::HealthIssue);
        req.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable due to failures".to_string()),
            severity: Some("warning".to_string()),
            ..PluginNotificationHealth::default()
        });

        let (payload, _) = passthrough(&req);
        assert_eq!(text_of(&payload)["title"], json!("IndexerStatusCheck"));
        assert_eq!(
            text_of(&payload)["description"],
            json!("Indexers unavailable due to failures")
        );
        assert_eq!(field_text(&payload, "Level").as_deref(), Some("warning"));
    }

    #[test]
    fn a_test_notification_carries_content_and_the_configured_instance_name() {
        let mut settings = Settings::passthrough();
        settings.instance_name = Some("Basement Scryer".to_string());
        let mut warnings = Vec::new();
        let payload = build_passthrough_payload(
            &request(NotificationEventType::Test),
            &settings,
            &mut warnings,
        );

        assert_eq!(
            text_of(&payload)["content"],
            json!("Test message from Basement Scryer posted at 2026-09-01T10:00:00Z")
        );
        assert!(
            text_of(&payload)["footer"]
                .as_str()
                .unwrap()
                .starts_with("Basement Scryer 0.19.8")
        );
    }

    #[test]
    fn a_daily_episode_uses_its_air_date_the_way_sonarr_does() {
        let mut req = full_request(NotificationEventType::Grab);
        let episode = PluginNotificationEpisode {
            episode_number: None,
            season_number: None,
            air_date: Some("2019-04-01".to_string()),
            title: Some("Ember".to_string()),
            ..PluginNotificationEpisode::default()
        };
        req.episode = Some(episode.clone());
        req.episodes = vec![episode];

        let (payload, _) = passthrough(&req);
        assert_eq!(
            text_of(&payload)["title"],
            json!("Cinder Line - 2019-04-01 - Ember")
        );
    }

    // -----------------------------------------------------------------------
    // Limits
    // -----------------------------------------------------------------------

    #[test]
    fn an_over_long_heading_is_truncated_with_a_warning() {
        let mut req = full_request(NotificationEventType::Grab);
        req.title.as_mut().unwrap().name = "N".repeat(400);
        req.episode = None;
        req.episodes = Vec::new();

        let (payload, warnings) = passthrough(&req);
        let title = text_of(&payload)["title"].as_str().unwrap();
        assert_eq!(title.chars().count(), EMBED_TITLE_LIMIT);
        assert!(title.ends_with('…'));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("title truncated")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_over_long_field_value_is_truncated_with_a_warning() {
        let mut req = full_request(NotificationEventType::Grab);
        req.release.as_mut().unwrap().source_title = Some("R".repeat(2000));

        let (payload, warnings) = passthrough(&req);
        let release = field_text(&payload, "Release").expect("release field");
        assert_eq!(release.chars().count(), EMBED_FIELD_VALUE_LIMIT);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("field text truncated")),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_embed_is_trimmed_to_discords_total_character_budget() {
        let mut payload = json!({
            "discord": { "text": {
                "title": "t",
                "description": "d",
                "fields": (0..20)
                    .map(|index| json!({
                        "title": format!("f{index}"),
                        "text": "x".repeat(500),
                        "inline": false,
                    }))
                    .collect::<Vec<_>>(),
            }}
        });
        let mut warnings = Vec::new();
        enforce_embed_limits(&mut payload, &mut warnings);

        assert!(embed_character_total(text_of(&payload)) <= EMBED_TOTAL_LIMIT);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("6000-character")),
            "{warnings:?}"
        );
    }

    #[test]
    fn format_bytes_matches_sonarrs_discord_formatter() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(1_073_741_824), "1 GB");
        assert_eq!(format_bytes(3_221_225_472), "3 GB");
    }

    // -----------------------------------------------------------------------
    // Sonarr-compatible payload
    // -----------------------------------------------------------------------

    #[test]
    fn the_sonarr_payload_is_camel_case_with_sonarrs_pascal_case_event_type() {
        let (payload, warnings) = sonarr(&full_request(NotificationEventType::Grab));

        assert_eq!(payload["eventType"], json!("Grab"));
        assert_eq!(payload["instanceName"], json!("Scryer"));
        assert_eq!(payload["series"]["title"], json!("Cinder Line"));
        assert_eq!(payload["series"]["titleSlug"], json!("cinder-line"));
        assert_eq!(payload["series"]["tvdbId"], json!(81189));
        assert_eq!(payload["series"]["tvMazeId"], json!(169));
        assert_eq!(payload["series"]["imdbId"], json!("tt0903747"));
        assert_eq!(payload["series"]["type"], json!("standard"));
        assert_eq!(payload["series"]["year"], json!(2019));
        assert_eq!(payload["series"]["tags"], json!(["4k"]));
        assert_eq!(payload["episodes"][0]["seasonNumber"], json!(2));
        assert_eq!(payload["episodes"][0]["episodeNumber"], json!(5));
        assert_eq!(payload["episodes"][0]["title"], json!("Ember"));
        assert_eq!(payload["release"]["quality"], json!("WEBDL-1080p"));
        assert_eq!(payload["release"]["releaseGroup"], json!("GRP"));
        assert_eq!(payload["release"]["indexer"], json!("Test Indexer"));
        assert_eq!(payload["downloadClient"], json!("Weaver"));
        assert_eq!(payload["downloadId"], json!("abc123"));
        assert_eq!(payload["customFormatInfo"]["customFormatScore"], json!(25));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// `WebhookBase.BuildOnImportCompletePayload` also stamps `Download`, with a
    /// file list and a count rather than a single file.
    #[test]
    fn import_complete_is_sonarrs_download_event_with_a_file_list() {
        let (payload, _) = sonarr(&full_request(NotificationEventType::ImportComplete));
        assert_eq!(payload["eventType"], json!("Download"));
        assert_eq!(payload["fileCount"], json!(1));
        assert_eq!(
            payload["episodeFiles"][0]["path"],
            json!("/media/TV/Cinder Line/S02/E05.mkv")
        );
        assert_eq!(
            payload["episodeFiles"][0]["mediaInfo"]["width"],
            json!(1920)
        );
        assert_eq!(
            payload["destinationPath"],
            json!("/media/TV/Cinder Line/S02/E05.mkv")
        );
        assert!(payload.get("episodeFile").is_none());
    }

    #[test]
    fn an_upgrade_is_sonarrs_download_event_with_the_upgrade_flag() {
        let (payload, _) = sonarr(&full_request(NotificationEventType::Upgrade));
        assert_eq!(payload["eventType"], json!("Download"));
        assert_eq!(payload["isUpgrade"], json!(true));
    }

    /// Sonarr's webhook schema has no failed-download event, so claiming
    /// `Download` would tell Notifiarr an episode imported.
    #[test]
    fn a_failed_download_becomes_manual_interaction_with_a_warning() {
        let mut req = full_request(NotificationEventType::Download);
        req.severity = Some(NotificationSeverity::Error);
        req.download.as_mut().unwrap().status = Some("failed".to_string());
        req.download.as_mut().unwrap().status_message = Some("all articles missing".to_string());

        let (payload, warnings) = sonarr(&req);
        assert_eq!(payload["eventType"], json!("ManualInteractionRequired"));
        assert_eq!(payload["downloadStatus"], json!("failed"));
        assert_eq!(
            payload["downloadStatusMessages"][0]["messages"][0],
            json!("all articles missing")
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("failed download")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_successful_download_stays_sonarrs_download_event() {
        let mut req = full_request(NotificationEventType::Download);
        req.summary_title = "Imported Cinder Line".to_string();
        let (payload, _) = sonarr(&req);
        assert_eq!(payload["eventType"], json!("Download"));
        assert_eq!(payload["isUpgrade"], json!(false));
        assert_eq!(
            payload["episodeFile"]["path"],
            json!("/media/TV/Cinder Line/S02/E05.mkv")
        );
    }

    #[test]
    fn every_sonarr_event_type_is_one_of_sonarrs_own_enum_members() {
        const WEBHOOK_EVENT_TYPES: [&str; 11] = [
            "Test",
            "Grab",
            "Download",
            "Rename",
            "SeriesAdd",
            "SeriesDelete",
            "EpisodeFileDelete",
            "Health",
            "ApplicationUpdate",
            "HealthRestored",
            "ManualInteractionRequired",
        ];
        for event_type in [
            NotificationEventType::Grab,
            NotificationEventType::Download,
            NotificationEventType::Upgrade,
            NotificationEventType::ImportComplete,
            NotificationEventType::ImportRejected,
            NotificationEventType::Rename,
            NotificationEventType::TitleAdded,
            NotificationEventType::TitleDeleted,
            NotificationEventType::FileDeleted,
            NotificationEventType::FileDeletedForUpgrade,
            NotificationEventType::HealthIssue,
            NotificationEventType::HealthRestored,
            NotificationEventType::ApplicationUpdate,
            NotificationEventType::ManualInteractionRequired,
            NotificationEventType::Test,
        ] {
            let mapped = sonarr_event_type(&request(event_type))
                .unwrap_or_else(|| panic!("{event_type:?} must map to a Sonarr event"));
            assert!(
                WEBHOOK_EVENT_TYPES.contains(&mapped),
                "{event_type:?} produced '{mapped}', which is not a WebhookEventType member"
            );
        }
    }

    /// The Scryer-only events have no member of `WebhookEventType` to occupy.
    /// Inventing one would be a guaranteed 400 on every send.
    #[test]
    fn a_scryer_only_event_is_unsupported_on_the_sonarr_integration() {
        for event_type in [
            NotificationEventType::PostProcessingCompleted,
            NotificationEventType::SubtitleDownloaded,
            NotificationEventType::SubtitleSearchFailed,
            NotificationEventType::MediaRequestSubmitted,
            NotificationEventType::MediaRequestApproved,
            NotificationEventType::MediaRequestRejected,
            NotificationEventType::MediaRequestCanceled,
        ] {
            assert!(
                sonarr_event_type(&request(event_type)).is_none(),
                "{event_type:?}"
            );
            let error =
                build_sonarr_payload(&request(event_type), &Settings::sonarr(), &mut Vec::new())
                    .expect_err("must be refused");
            assert_eq!(error.code, PluginErrorCode::Unsupported);
            assert!(
                error.public_message.contains(INTEGRATION_PASSTHROUGH),
                "the operator has to be told where to get the event: {error:?}"
            );
        }
    }

    /// The lie the brief calls out: Notifiarr's Sonarr integration only models
    /// series. Sending it anyway is the operator's choice; hiding it is not.
    #[test]
    fn a_movie_on_the_sonarr_integration_warns_that_it_is_sent_as_a_series() {
        let mut req = full_request(NotificationEventType::Grab);
        req.title.as_mut().unwrap().facet = "movie".to_string();

        let (payload, warnings) = sonarr(&req);
        assert_eq!(payload["series"]["title"], json!("Cinder Line"));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("only models series")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_anime_facet_uses_sonarrs_anime_series_type() {
        let mut req = full_request(NotificationEventType::Grab);
        req.title.as_mut().unwrap().facet = "anime".to_string();
        let (payload, warnings) = sonarr(&req);
        assert_eq!(payload["series"]["type"], json!("anime"));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_file_delete_carries_sonarrs_camel_case_delete_reason() {
        let (upgrade, _) = sonarr(&full_request(NotificationEventType::FileDeletedForUpgrade));
        assert_eq!(upgrade["eventType"], json!("EpisodeFileDelete"));
        assert_eq!(upgrade["deleteReason"], json!("upgrade"));

        let (manual, _) = sonarr(&full_request(NotificationEventType::FileDeleted));
        assert_eq!(manual["deleteReason"], json!("manual"));
    }

    #[test]
    fn a_health_payload_uses_sonarrs_camel_case_level() {
        let mut req = request(NotificationEventType::HealthIssue);
        req.severity = Some(NotificationSeverity::Warning);
        req.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable".to_string()),
            ..PluginNotificationHealth::default()
        });

        let (payload, _) = sonarr(&req);
        assert_eq!(payload["eventType"], json!("Health"));
        assert_eq!(payload["level"], json!("warning"));
        assert_eq!(payload["message"], json!("Indexers unavailable"));
        assert_eq!(payload["type"], json!("IndexerStatusCheck"));
    }

    #[test]
    fn a_rename_carries_previous_and_new_paths() {
        let mut req = full_request(NotificationEventType::Rename);
        req.media_files[0].previous_path = Some("/media/TV/Cinder Line/S02/old.mkv".to_string());

        let (payload, _) = sonarr(&req);
        assert_eq!(payload["eventType"], json!("Rename"));
        assert_eq!(
            payload["renamedEpisodeFiles"][0]["previousPath"],
            json!("/media/TV/Cinder Line/S02/old.mkv")
        );
        assert_eq!(
            payload["renamedEpisodeFiles"][0]["path"],
            json!("/media/TV/Cinder Line/S02/E05.mkv")
        );
    }

    /// `NullValueHandling.Ignore`: Sonarr never emits a null member, so neither
    /// does this.
    #[test]
    fn the_sonarr_payload_never_emits_a_null_member() {
        fn assert_no_nulls(value: &Value, path: &str) {
            match value {
                Value::Null => panic!("null at {path}"),
                Value::Object(map) => {
                    for (key, child) in map {
                        assert_no_nulls(child, &format!("{path}.{key}"));
                    }
                }
                Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        assert_no_nulls(child, &format!("{path}[{index}]"));
                    }
                }
                _ => {}
            }
        }
        for event_type in [
            NotificationEventType::Grab,
            NotificationEventType::ImportComplete,
            NotificationEventType::Rename,
            NotificationEventType::Test,
        ] {
            let (sparse, _) = sonarr(&request(event_type));
            assert_no_nulls(&sparse, "sparse");
            let (full, _) = sonarr(&full_request(event_type));
            assert_no_nulls(&full, "full");
        }
    }

    /// Scryer's ids are opaque strings; Sonarr's are ints. A non-numeric id
    /// becomes 0 rather than a type Notifiarr's parser rejects.
    #[test]
    fn a_non_numeric_scryer_id_becomes_sonarrs_zero() {
        let mut req = full_request(NotificationEventType::Grab);
        req.title.as_mut().unwrap().id = Some("018f2c1a-title".to_string());
        let (payload, _) = sonarr(&req);
        assert_eq!(payload["series"]["id"], json!(0));
        assert!(payload["series"]["id"].is_number());
    }

    // -----------------------------------------------------------------------
    // Delivery classification
    // -----------------------------------------------------------------------

    fn classify(status: u16, body: &str) -> PluginResult<PluginNotificationResponse> {
        classify_response(
            status,
            &BTreeMap::new(),
            body.as_bytes(),
            &Settings::passthrough(),
            Vec::new(),
        )
    }

    fn ok(result: PluginResult<PluginNotificationResponse>) -> PluginNotificationResponse {
        match result {
            PluginResult::Ok(response) => response,
            PluginResult::Err(error) => panic!("expected a delivery result, got {error:?}"),
        }
    }

    fn err(result: PluginResult<PluginNotificationResponse>) -> PluginError {
        match result {
            PluginResult::Err(error) => error,
            PluginResult::Ok(response) => panic!("expected a typed error, got {response:?}"),
        }
    }

    #[test]
    fn a_delivered_notification_reports_its_target() {
        let response = ok(classify(
            200,
            r#"{"result":"success","details":{"response":"notification sent"}}"#,
        ));
        assert!(response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_200"));
        assert_eq!(
            response.target_results[0].target,
            "notifiarr:passthrough/910000000000000001"
        );
        assert!(response.target_results[0].success);
    }

    /// The `Script::default()` shape of the conformance harness, and of any
    /// proxy that answers `{}`: no `result` member means the status code is the
    /// whole answer.
    #[test]
    fn a_two_hundred_with_no_result_member_is_a_success() {
        assert!(ok(classify(200, "{}")).success);
        assert!(ok(classify(204, "")).success);
    }

    /// Notifiarr answers 200 with `result: error` when the integration itself
    /// refuses. Sonarr never reads the body and reports these as delivered.
    #[test]
    fn a_two_hundred_carrying_an_error_result_is_a_delivery_failure() {
        let response = ok(classify(
            200,
            r#"{"result":"error","details":{"response":"channel not found"}}"#,
        ));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("notifiarr_error"));
        assert!(
            response
                .error
                .as_deref()
                .unwrap()
                .contains("channel not found"),
            "{response:?}"
        );
    }

    /// `NotifiarrProxy.cs:178-180`.
    #[test]
    fn a_401_names_the_api_key_setting() {
        let error = err(classify(
            401,
            r#"{"result":"error","message":"invalid api key"}"#,
        ));
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("api_key"), "{error:?}");
        assert!(
            error.public_message.contains("invalid api key"),
            "{error:?}"
        );

        assert_eq!(
            err(classify(403, r#"{"result":"error"}"#)).code,
            PluginErrorCode::AuthFailed
        );
    }

    /// Sonarr swallows a 400 (`NotifiarrProxy.cs:181-185`) and the June port
    /// turned it into `success: true`. It is a failed delivery.
    #[test]
    fn a_400_is_a_reported_delivery_failure_carrying_notifiarrs_own_text() {
        let response = ok(classify(
            400,
            r#"{"result":"error","details":{"response":"passthrough integration not enabled"}}"#,
        ));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_400"));
        let error = response.error.unwrap();
        assert!(
            error.contains("passthrough integration not enabled"),
            "{error}"
        );
        assert!(
            error.contains("Passthrough integration on notifiarr.com"),
            "{error}"
        );
    }

    #[test]
    fn a_400_on_the_sonarr_integration_points_at_the_sonarr_integration() {
        let response = ok(classify_response(
            400,
            &BTreeMap::new(),
            br#"{"result":"error","details":{"response":"no"}}"#,
            &Settings::sonarr(),
            Vec::new(),
        ));
        assert!(
            response
                .error
                .as_deref()
                .unwrap()
                .contains("Sonarr integration on notifiarr.com"),
            "{response:?}"
        );
        assert_eq!(response.target_results[0].target, "notifiarr:sonarr");
    }

    /// `NotifiarrProxy.cs:186-196` — Notifiarr down, then Cloudflare's own
    /// five-hundreds.
    #[test]
    fn service_and_cloudflare_failures_stay_on_the_delivery_lane() {
        for status in [502, 503, 504] {
            let response = ok(classify(status, r#"{"result":"error"}"#));
            assert!(!response.success);
            assert!(
                response.error.as_deref().unwrap().contains("unavailable"),
                "{status}: {response:?}"
            );
        }
        for status in [520, 521, 522, 523, 524] {
            let response = ok(classify(status, r#"{"result":"error"}"#));
            assert!(!response.success);
            assert!(
                response.error.as_deref().unwrap().contains("Cloudflare"),
                "{status}: {response:?}"
            );
            assert_eq!(
                response.provider_status.as_deref(),
                Some(format!("http_{status}").as_str())
            );
        }
    }

    /// A non-JSON non-2xx did not come from Notifiarr's API; the vendor's own
    /// client special-cases exactly this (`website.go:53-60`).
    #[test]
    fn a_cloudflare_html_page_is_blamed_on_the_edge_not_on_the_api_key() {
        let response = ok(classify(
            400,
            "<html><title>Attention Required!</title></html>",
        ));
        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .unwrap()
                .contains("something in front of Notifiarr"),
            "{response:?}"
        );
    }

    #[test]
    fn a_429_reports_the_retry_after_the_core_can_act_on() {
        let headers = BTreeMap::from([("Retry-After".to_string(), "90".to_string())]);
        let response = ok(classify_response(
            429,
            &headers,
            br#"{"result":"error","details":{"response":"rate limited"}}"#,
            &Settings::passthrough(),
            Vec::new(),
        ));
        assert!(!response.success);
        assert_eq!(response.retry_after_seconds, Some(90));
        assert_eq!(response.provider_status.as_deref(), Some("http_429"));
    }

    #[test]
    fn a_404_names_the_integration_setting() {
        let error = err(classify(404, r#"{"result":"error"}"#));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("passthrough"), "{error:?}");
    }

    #[test]
    fn notifiarr_bodies_are_parsed_from_every_documented_shape() {
        assert!(parse_notifiarr_body(br#"{"result":"success"}"#).succeeded());
        assert_eq!(
            parse_notifiarr_body(br#"{"result":"error","details":{"help":"try again"}}"#)
                .detail(400),
            "try again"
        );
        assert_eq!(
            parse_notifiarr_body(br#"{"message":"bad request"}"#).detail(400),
            "bad request"
        );
        assert_eq!(
            parse_notifiarr_body(br#"{"result":"error","details":{"response":["a","b"]}}"#)
                .detail(400),
            "a; b"
        );
        let html = parse_notifiarr_body(b"<html>nope</html>");
        assert!(!html.is_json);
        assert!(
            html.succeeded(),
            "a non-JSON body says nothing about the result"
        );
    }
}
