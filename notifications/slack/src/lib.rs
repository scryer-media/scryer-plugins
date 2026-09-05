//! Slack incoming-webhook notifications, as a WASI Preview 2 component.
//!
//! # What this channel owes the operator
//!
//! Sonarr's Slack notification (`src/NzbDrone.Core/Notifications/Slack/Slack.cs`)
//! posts one legacy attachment per event: a `title`, the event's prose message
//! as `text`, and a `color` from a three-value table. The June port copied that
//! shape but lost the two things in it that carry information — Sonarr's
//! per-event heading (`Slack.GetTitle`, `Slack.cs:267-291`, which composes
//! "Series - 2x03 - Episode title" for a file delete) and the fact that
//! `NotificationEventType::Download` in Scryer is a **failed** download, which
//! the port rendered as "Imported: …".
//!
//! This module rebuilds the channel on Scryer's notification contract:
//!
//! * the heading follows `Slack.GetTitle` generalised to Scryer's facets, and is
//!   used for every event rather than only the delete ones;
//! * the attachment body is enriched with the structured blocks the contract
//!   carries (episode, quality, release, indexer, client, sizes, paths, health,
//!   versions) as Block Kit `section` fields — Sonarr's Slack channel has no
//!   equivalent because Sonarr hands the channel one prose sentence;
//! * the colour table is Sonarr's `good`/`warning`/`danger`, extended over
//!   Scryer's larger event enum, with `severity` as an override Sonarr has no
//!   equivalent for;
//! * Slack's own error strings are mapped to the offending **configuration
//!   field**, which Sonarr cannot do at all (`SlackProxy.cs:39-43` turns every
//!   HTTP failure into one `SlackExeption`).
//!
//! # Why Block Kit inside a legacy attachment
//!
//! Slack marks secondary message attachments legacy — "This feature is a legacy
//! part of messaging functionality for Slack apps […] we recommend you stick
//! with layout blocks"
//! (<https://docs.slack.dev/legacy/legacy-messaging/legacy-secondary-message-attachments/>)
//! — and warns they "may change in the future in ways that reduce their
//! visibility or utility". Pure Block Kit, however, has no colour: the coloured
//! left border Sonarr operators read at a glance exists **only** on an
//! attachment. Slack's own documented bridge is an attachment that carries
//! `color` plus a `blocks` array, so that is what this channel sends: the
//! attachment supplies nothing but `color` and `fallback`, and every piece of
//! content is a modern block. None of the legacy content fields
//! (`title`, `text`, `fields`, `author_name`, `pretext`, `mrkdwn_in`) is used,
//! so the day Slack reduces them this channel loses a border and nothing else.
//!
//! # Why the delivery path is local rather than `notify_common::send_json`
//!
//! The shared helper collapses every non-2xx into `error_response("HTTP N:
//! body", "http_N")`. Slack's incoming webhooks answer with a **plain-text**
//! error string and a meaningful status
//! (<https://docs.slack.dev/changelog/2016-05-17-changes-to-errors-for-incoming-webhooks/>):
//! `invalid_payload` at 400 is a bug in the payload this plugin built,
//! `no_service` at 404 is a webhook URL that no longer exists,
//! `channel_is_archived` at 410 is the `channel` setting, `action_prohibited` at
//! 403 is a workspace restriction, and a 429 carries a `Retry-After` the core
//! can act on. Those are three different lanes in Scryer's contract — typed
//! `PluginError`, delivery failure with `retry_after_seconds`, and plain
//! delivery failure — so the send lives here.
//!
//! # Upstream reference
//!
//! Read 2026-09-01:
//! * <https://docs.slack.dev/messaging/sending-messages-using-incoming-webhooks>
//!   (payload fields, the error-string table, and the statement that channel,
//!   username and icon **cannot** be overridden by a Slack-app webhook);
//! * <https://docs.slack.dev/legacy/legacy-messaging/legacy-secondary-message-attachments/>
//!   (attachment legacy status and field reference);
//! * <https://docs.slack.dev/apis/web-api/rate-limits/> (1 request per second per
//!   webhook, short bursts allowed, 429 + `Retry-After`);
//! * <https://docs.slack.dev/messaging/formatting-message-text> (`&`, `<`, `>`
//!   are control characters and must be HTML-escaped);
//! * <https://docs.slack.dev/reference/methods/chat.postMessage/> (message text
//!   is truncated past 40,000 characters; 4,000 is the documented practical
//!   cap).

use std::collections::BTreeMap;

use notify_common::*;
use scryer_plugin_sdk::{
    NotificationDescriptor, NotificationEventOptions, NotificationSeverity,
    PluginNotificationEpisode, current_sdk_constraint,
};
use serde_json::{Value, json};

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

const PROVIDER_TYPE: &str = "slack";
const USER_AGENT: &str = concat!("scryer-slack-plugin/", env!("CARGO_PKG_VERSION"));

/// The display name a webhook with no configured `username` posts under.
///
/// Sonarr requires `Username` and offers no default (`SlackSettings.cs:12`); the
/// descriptor carries this as the field default, and the renderer applies the
/// same value when the host has not resolved one.
const DEFAULT_USERNAME: &str = "Scryer";

// ---------------------------------------------------------------------------
// Slack's documented limits
// ---------------------------------------------------------------------------

/// `chat.postMessage` → `text`: "For best results, limit the number of
/// characters in the `text` field to 4,000 characters"; Slack truncates past
/// 40,000. Trimming to the documented practical cap keeps the message intact
/// instead of letting Slack cut it at an arbitrary point.
const TEXT_LIMIT: usize = 4_000;

/// Section block `text`: "Maximum length for the text in this field is 3000
/// characters" (<https://docs.slack.dev/reference/block-kit/blocks/section-block/>).
const SECTION_TEXT_LIMIT: usize = 3_000;

/// Section block `fields`: at most 10 items, each at most 2000 characters.
const SECTION_FIELD_LIMIT: usize = 2_000;
const SECTION_FIELD_COUNT_LIMIT: usize = 10;

/// Context block: at most 10 elements.
const CONTEXT_ELEMENT_LIMIT: usize = 10;

/// At most 50 blocks per message or attachment.
const BLOCK_COUNT_LIMIT: usize = 50;

/// Not a Slack limit: a heading longer than this stops being a heading. Sonarr's
/// own Discord sibling caps at the same number, and Slack collapses attachment
/// text past 700 characters anyway.
const HEADING_LIMIT: usize = 256;

// ---------------------------------------------------------------------------
// Colours (Slack.cs:36, 53, 71, 148, 165, 182, 199)
// ---------------------------------------------------------------------------

const COLOR_GOOD: &str = "good";
const COLOR_WARNING: &str = "warning";
const COLOR_DANGER: &str = "danger";

/// Sonarr leaves `Color` unset for rename, delete and series-add, which renders
/// a colourless grey bar. Slack accepts a hex value, and `#439FE0` is the blue
/// its own attachment reference uses for an informational message, so
/// informational events get that instead of nothing.
const COLOR_INFO: &str = "#439FE0";

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

/// Built here rather than through `notify_common::build_notification_descriptor`
/// because that helper cannot express `event_options`. Sonarr's Slack channel
/// implements `OnDownload`, `OnEpisodeFileDelete` and `OnHealthIssue`, and
/// `NotificationBase` derives `SupportsOnUpgrade` from the first and
/// `SupportsOnEpisodeFileDeleteForUpgrade` from the second
/// (`NotificationBase.cs:96-108`), so all three operator-facing event filters
/// are meaningful for this channel.
fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "Slack".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec![],
            default_base_url: None,
            // Deliberately unrestricted. Slack's own webhooks live on
            // `hooks.slack.com`, but the incoming-webhook payload is a de facto
            // format that Mattermost, Rocket.Chat and Discord's `/slack`
            // compatibility endpoint all accept, and operators point this
            // channel at them. An allowlist here would break those with no
            // security gain the host's egress policy does not already provide.
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                supports_rich_text: true,
                // Deliberately false; see `build_blocks`. Slack fetches an image
                // URL server-side, so a poster served from a private Scryer
                // instance either fails to render or costs the whole message.
                supports_images: false,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes: vec![
                    NotificationDeliveryMode::Chat,
                    NotificationDeliveryMode::Webhook,
                ],
                payload_formats: vec![
                    NotificationPayloadFormat::PlainText,
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
        connection_field(
            "webhook_url",
            "Webhook URL",
            true,
            None,
            Some(
                "The Slack incoming-webhook URL, e.g. https://hooks.slack.com/services/T0/B0/token.",
            ),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            true,
            Some(DEFAULT_USERNAME),
            Some(
                "Display name for the message. Ignored by webhooks created from a Slack app, which always post as the app.",
            ),
        ),
        field(
            "icon",
            "Icon",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "An emoji name wrapped in colons (:robot_face:) or an http(s) image URL. Ignored by webhooks created from a Slack app.",
            ),
        ),
        field(
            "channel",
            "Channel",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Channel override, e.g. #media. Only legacy custom-integration webhooks honour it; a Slack-app webhook always posts to the channel chosen at install.",
            ),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// The webhook identity overrides Slack accepts on the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Icon {
    /// `:emoji_name:` → `icon_emoji`, per `Slack.CreatePayload` (`Slack.cs:246-257`).
    Emoji(String),
    /// Anything else → `icon_url`, same branch.
    Url(String),
}

/// Everything the renderer needs from configuration, resolved and validated once
/// per send so every builder below is a pure function of `(request, settings)`
/// and therefore testable without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    webhook_url: String,
    username: String,
    icon: Option<Icon>,
    channel: Option<String>,
    /// An `icon` that is neither `:emoji:` nor a URL, kept so a live send can
    /// warn about it instead of failing.
    icon_warning: Option<String>,
}

impl Settings {
    /// `strict` is the Test-time posture: an `icon` Slack cannot interpret is
    /// refused so the operator sees it in the connection test. On a live send
    /// the same value is dropped with a warning instead — a decorative setting
    /// must never stop notifications.
    fn from_config(strict: bool) -> Result<Self, PluginError> {
        let webhook_url = required_config("webhook_url").map_err(config_error)?;
        validate_webhook_url(&webhook_url)?;
        let username = validated_username(config::get("username").ok().flatten().as_deref())?;
        let (icon, icon_warning) = validated_icon(config_value("icon").as_deref(), strict)?;

        Ok(Self {
            webhook_url,
            username,
            icon,
            channel: config_value("channel"),
            icon_warning,
        })
    }

    #[cfg(test)]
    fn defaults() -> Self {
        Self {
            webhook_url: "https://hooks.slack.com/services/T0/B0/token".to_string(),
            username: DEFAULT_USERNAME.to_string(),
            icon: None,
            channel: None,
            icon_warning: None,
        }
    }
}

/// `SlackSettingsValidator` (`SlackSettings.cs:11`): `RuleFor(c => c.WebHookUrl).IsValidUrl()`.
///
/// Sonarr can only say this at settings-validation time; the June port sent the
/// value to the host and reported whatever came back, which told the operator a
/// message failed rather than that a setting is wrong.
fn validate_webhook_url(url: &str) -> Result<(), PluginError> {
    let lowercase = url.to_ascii_lowercase();
    if lowercase.starts_with("https://") || lowercase.starts_with("http://") {
        return Ok(());
    }
    Err(plugin_error(
        PluginErrorCode::InvalidConfig,
        "webhook_url must be an http(s) Slack incoming-webhook URL".to_string(),
        Some(format!("configured value: {url}")),
    ))
}

/// `SlackSettingsValidator` (`SlackSettings.cs:12`): `RuleFor(c => c.Username).NotEmpty()`.
///
/// The raw value is read rather than `config_value`, because a key the host has
/// not resolved at all (so the descriptor default applies) and a key the
/// operator has blanked are different intentions and `config_value` filters both
/// to `None`.
fn validated_username(raw: Option<&str>) -> Result<String, PluginError> {
    match raw {
        None => Ok(DEFAULT_USERNAME.to_string()),
        Some(value) if value.trim().is_empty() => Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "username must not be empty".to_string(),
            Some("Slack posts the webhook under this display name".to_string()),
        )),
        Some(value) => Ok(value.trim().to_string()),
    }
}

/// `Slack.CreatePayload` (`Slack.cs:246-257`) picks `icon_emoji` when the value
/// is wrapped in colons and `icon_url` otherwise — including for values that are
/// not URLs at all, which Slack then rejects with `invalid_payload`.
fn validated_icon(
    raw: Option<&str>,
    strict: bool,
) -> Result<(Option<Icon>, Option<String>), PluginError> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok((None, None));
    };

    if value.starts_with(':') && value.ends_with(':') && value.len() > 2 {
        return Ok((Some(Icon::Emoji(value.to_string())), None));
    }

    let lowercase = value.to_ascii_lowercase();
    if lowercase.starts_with("https://") || lowercase.starts_with("http://") {
        return Ok((Some(Icon::Url(value.to_string())), None));
    }

    if strict {
        return Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            "icon must be an emoji name wrapped in colons (:robot_face:) or an http(s) image URL"
                .to_string(),
            Some(format!("configured value: {value}")),
        ));
    }

    Ok((
        None,
        Some(format!(
            "icon {value:?} is neither an emoji name in colons nor an http(s) URL; it was not sent"
        )),
    ))
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// Build the Slack webhook payload plus any degradation warnings.
///
/// `Slack.CreatePayload` (`Slack.cs:234-265`) with the identity fields in the
/// same order and the same emoji-vs-URL rule; the content is a Block Kit
/// attachment rather than the legacy `title`/`text`/`fields` triple.
fn build_payload(req: &PluginNotificationRequest, settings: &Settings) -> (Value, Vec<String>) {
    let mut warnings = Vec::new();
    if let Some(warning) = settings.icon_warning.clone() {
        warnings.push(warning);
    }

    let mut payload = serde_json::Map::new();

    if req.is_test || req.event_type == NotificationEventType::Test {
        // Sonarr's test is a plain text message with a timestamp and no
        // attachment (`Slack.cs:217-232`), which is also the cheapest possible
        // proof that the webhook URL is live.
        payload.insert("text".to_string(), json!(test_message(req)));
    } else {
        payload.insert("text".to_string(), json!(notification_text(req)));
        payload.insert(
            "attachments".to_string(),
            json!([build_attachment(req, &mut warnings)]),
        );
    }

    payload.insert("username".to_string(), json!(settings.username));
    match &settings.icon {
        Some(Icon::Emoji(emoji)) => {
            payload.insert("icon_emoji".to_string(), json!(emoji));
        }
        Some(Icon::Url(url)) => {
            payload.insert("icon_url".to_string(), json!(url));
        }
        None => {}
    }
    if let Some(channel) = &settings.channel {
        payload.insert("channel".to_string(), json!(channel));
    }

    let mut payload = Value::Object(payload);
    enforce_limits(&mut payload, &mut warnings);
    (payload, warnings)
}

fn test_message(req: &PluginNotificationRequest) -> String {
    // Sonarr stamps `DateTime.Now` (`Slack.cs:221`). A component has no
    // guaranteed wall clock, so the event's own timestamp is used when the core
    // sends one — `enrich_notification` always does — and the sentence simply
    // ends early when it does not.
    let app = mrkdwn_escape(req.app.name.trim());
    match req.occurred_at.as_deref().map(str::trim) {
        Some(occurred_at) if !occurred_at.is_empty() => {
            format!(
                "Test message from {app} posted at {}",
                mrkdwn_escape(occurred_at)
            )
        }
        _ => format!("Test message from {app}"),
    }
}

/// The message body Slack shows in the channel list and the push notification.
///
/// Sonarr composes it as `"{verb}: {message}"` per event
/// (`Slack.cs:40, 57, 74, 89, 104, …`). Scryer's dispatcher already puts an
/// event-specific heading in `summary_title` ("Grabbed: X", "Import complete:
/// X", "Download failed: X"), so that is the line — which is also what keeps
/// this channel honest about `NotificationEventType::Download`.
fn notification_text(req: &PluginNotificationRequest) -> String {
    let summary = req.summary_title.trim();
    if !summary.is_empty() {
        return mrkdwn_escape(summary);
    }
    mrkdwn_escape(&format!("{}: {}", event_label(req), heading(req)))
}

fn build_attachment(req: &PluginNotificationRequest, warnings: &mut Vec<String>) -> Value {
    let mut attachment = serde_json::Map::new();
    attachment.insert("color".to_string(), json!(attachment_color(req)));
    attachment.insert("fallback".to_string(), json!(fallback_text(req)));
    attachment.insert("blocks".to_string(), Value::Array(build_blocks(req)));
    let mut attachment = Value::Object(attachment);
    enforce_block_limits(&mut attachment, warnings);
    attachment
}

/// "A plain text summary of the attachment used in clients that don't show
/// formatted text" (legacy attachment reference). Heading plus body, with no
/// markup.
fn fallback_text(req: &PluginNotificationRequest) -> String {
    let heading = heading(req);
    let body = attachment_body(req);
    let raw = if body.is_empty() {
        heading
    } else {
        format!("{heading} - {body}")
    };
    mrkdwn_escape(&raw)
}

/// The blocks the attachment carries.
///
/// No `image` block and no image accessory: Slack fetches an image URL
/// server-side, and Scryer's poster URLs routinely point at an instance Slack
/// cannot reach, which renders nothing at best and costs the whole message at
/// worst. The descriptor says `supports_images: false` for the same reason, and
/// Sonarr's Slack channel sends no image either.
fn build_blocks(req: &PluginNotificationRequest) -> Vec<Value> {
    let mut blocks = Vec::new();

    let heading = mrkdwn_escape(&heading(req));
    let body = mrkdwn_escape(&attachment_body(req));
    let headline = if body.is_empty() {
        format!("*{heading}*")
    } else {
        format!("*{heading}*\n{body}")
    };
    blocks.push(json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": headline },
    }));

    let fields = event_fields(req);
    if !fields.is_empty() {
        blocks.push(json!({
            "type": "section",
            "fields": fields
                .iter()
                .map(|(label, value)| json!({
                    "type": "mrkdwn",
                    "text": format!("*{}*\n{}", mrkdwn_escape(label), mrkdwn_escape(value)),
                }))
                .collect::<Vec<_>>(),
        }));
    }

    let mut context: Vec<Value> = Vec::new();
    if let Some(links) = links_string(req) {
        context.push(json!({ "type": "mrkdwn", "text": links }));
    }
    context.push(json!({ "type": "mrkdwn", "text": provenance_line(req) }));
    blocks.push(json!({ "type": "context", "elements": context }));

    blocks
}

/// The greyed footer line. Sonarr has no equivalent because a Sonarr instance
/// is the only thing posting to its webhook; Scryer names itself so an operator
/// running several instances can tell them apart, and stamps the event time the
/// dispatcher always supplies.
fn provenance_line(req: &PluginNotificationRequest) -> String {
    let app = mrkdwn_escape(req.app.name.trim());
    match req.occurred_at.as_deref().map(str::trim) {
        Some(occurred_at) if !occurred_at.is_empty() => {
            format!("{app} · {}", mrkdwn_escape(occurred_at))
        }
        _ => app,
    }
}

// ---------------------------------------------------------------------------
// Heading, body, colour
// ---------------------------------------------------------------------------

fn summary(req: &PluginNotificationRequest) -> String {
    let summary = req.summary_title.trim();
    if summary.is_empty() {
        req.app.name.trim().to_string()
    } else {
        summary.to_string()
    }
}

/// `Slack.GetTitle` (`Slack.cs:267-291`) on Scryer's contract, used for every
/// event rather than only the delete ones.
///
/// Sonarr composes "Series - {season}x{ep}[x{ep}…] - Episode titles", or
/// "Series - {air date} - Episode title" for a daily series, and passes the bare
/// series title everywhere else. Scryer's contract already carries a rendered
/// `episode.display` for most events; when it does not, the episode list is
/// composed the same way Sonarr composes it.
///
/// Health, application-update and manual-interaction events are the exception:
/// Sonarr heads those with `healthCheck.Source.Name` and `Environment.MachineName`
/// (`Slack.cs:146, 180, 197`). The contract carries the health source, but has
/// no carrier for a machine name at all, so `app.name` stands in — see the
/// report's out-of-fence findings.
fn heading(req: &PluginNotificationRequest) -> String {
    let raw = match req.event_type {
        NotificationEventType::HealthIssue => health_source(req).unwrap_or_else(|| summary(req)),
        NotificationEventType::HealthRestored => health_source(req).unwrap_or_else(|| summary(req)),
        NotificationEventType::ApplicationUpdate
        | NotificationEventType::ManualInteractionRequired => req.app.name.trim().to_string(),
        _ => {
            let name = req
                .title
                .as_ref()
                .map(|title| title.name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| summary(req));
            match episode_detail(req) {
                Some(detail) => format!("{name} - {detail}"),
                None => name,
            }
        }
    };

    ellipsize(&raw, HEADING_LIMIT)
}

fn episode_detail(req: &PluginNotificationRequest) -> Option<String> {
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

    let titles: Vec<&str> = episodes
        .iter()
        .filter_map(|episode| episode.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .collect();
    let titles = titles.join(" + ");

    // Sonarr's daily-series branch keys off `SeriesTypes.Daily` (`Slack.cs:279`);
    // the contract has no series type, so the observable stand-in is an episode
    // that has an air date and no episode number.
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

/// The attachment's prose line. Sonarr hands the event's own message straight
/// through (`Slack.cs:35, 52, 69`), and Scryer's `summary_message` is the same
/// sentence — plus, for several events, the only carrier for a fact no
/// structured block holds (the delete reason, the "imported N files" count).
fn attachment_body(req: &PluginNotificationRequest) -> String {
    let message = req.summary_message.trim();
    match req.event_type {
        NotificationEventType::HealthIssue => health_message(req)
            .unwrap_or_else(|| message.to_string())
            .trim()
            .to_string(),
        // `Slack.OnHealthRestored` (`Slack.cs:164`).
        NotificationEventType::HealthRestored => {
            let detail = health_message(req).unwrap_or_else(|| message.to_string());
            let detail = detail.trim();
            if detail.is_empty() {
                "The issue is now resolved".to_string()
            } else {
                format!("The following issue is now resolved: {detail}")
            }
        }
        NotificationEventType::ApplicationUpdate => req
            .application_update
            .as_ref()
            .and_then(|update| update.summary.clone())
            .map(|summary| summary.trim().to_string())
            .filter(|summary| !summary.is_empty())
            .unwrap_or_else(|| message.to_string()),
        _ => {
            if message.is_empty() {
                event_label(req)
            } else {
                message.to_string()
            }
        }
    }
}

/// Sonarr's per-event verb. Sonarr's wording is series-specific because Sonarr
/// only has series; Scryer carries a facet, so the episode wording is kept where
/// the facet is episodic and neutral wording is used otherwise.
fn event_label(req: &PluginNotificationRequest) -> String {
    let episodic = is_episodic(req);
    match req.event_type {
        NotificationEventType::Grab => episodic_label(episodic, "Episode Grabbed", "Grabbed"),
        // Never "Imported": the dispatcher maps a FAILED download onto this
        // event (`dispatcher.rs:34, 418-448`, verified on release-0.19.8 and
        // release-NEXT). A successful import is `ImportComplete` or `Upgrade`.
        NotificationEventType::Download => "Download Failed".to_string(),
        NotificationEventType::Upgrade => episodic_label(episodic, "Episode Upgraded", "Upgraded"),
        NotificationEventType::ImportComplete => {
            episodic_label(episodic, "Episode Imported", "Import Complete")
        }
        NotificationEventType::ImportRejected => "Import Rejected".to_string(),
        NotificationEventType::Rename => "Renamed".to_string(),
        NotificationEventType::FileDeleted => {
            episodic_label(episodic, "Episode Deleted", "File Deleted")
        }
        NotificationEventType::FileDeletedForUpgrade => episodic_label(
            episodic,
            "Episode Deleted for Upgrade",
            "File Deleted for Upgrade",
        ),
        NotificationEventType::TitleAdded => episodic_label(episodic, "Series Added", "Added"),
        NotificationEventType::TitleDeleted => {
            episodic_label(episodic, "Series Deleted", "Deleted")
        }
        NotificationEventType::ManualInteractionRequired => {
            "Manual Interaction Required".to_string()
        }
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

fn episodic_label(episodic: bool, episodic_label: &str, neutral_label: &str) -> String {
    if episodic {
        episodic_label.to_string()
    } else {
        neutral_label.to_string()
    }
}

fn is_episodic(req: &PluginNotificationRequest) -> bool {
    req.title
        .as_ref()
        .map(|title| title.facet.to_ascii_lowercase())
        .is_some_and(|facet| matches!(facet.as_str(), "series" | "anime" | "tv" | "show"))
}

/// Sonarr's three-value colour table (`Slack.cs:36, 53, 71, 148, 165, 182, 199`)
/// extended over Scryer's larger event enum, with `severity` as an override
/// Sonarr has no equivalent for. A warning never downgrades an already-red
/// event.
fn attachment_color(req: &PluginNotificationRequest) -> &'static str {
    let base = match req.event_type {
        NotificationEventType::Grab
        | NotificationEventType::ManualInteractionRequired
        | NotificationEventType::HealthIssue => COLOR_WARNING,
        NotificationEventType::Download
        | NotificationEventType::ImportRejected
        | NotificationEventType::FileDeleted
        | NotificationEventType::FileDeletedForUpgrade
        | NotificationEventType::TitleDeleted
        | NotificationEventType::SubtitleSearchFailed
        | NotificationEventType::MediaRequestRejected => COLOR_DANGER,
        NotificationEventType::Upgrade
        | NotificationEventType::ImportComplete
        | NotificationEventType::TitleAdded
        | NotificationEventType::HealthRestored
        | NotificationEventType::ApplicationUpdate
        | NotificationEventType::PostProcessingCompleted
        | NotificationEventType::SubtitleDownloaded
        | NotificationEventType::MediaRequestApproved => COLOR_GOOD,
        NotificationEventType::Rename
        | NotificationEventType::MediaRequestSubmitted
        | NotificationEventType::MediaRequestCanceled
        | NotificationEventType::Test => COLOR_INFO,
    };

    match req.severity {
        Some(NotificationSeverity::Error) => COLOR_DANGER,
        Some(NotificationSeverity::Warning) if base != COLOR_DANGER => COLOR_WARNING,
        _ => base,
    }
}

fn health_source(req: &PluginNotificationRequest) -> Option<String> {
    req.health.as_ref().and_then(|health| {
        health
            .code
            .clone()
            .or_else(|| health.status.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn health_message(req: &PluginNotificationRequest) -> Option<String> {
    req.health
        .as_ref()
        .and_then(|health| health.message.clone())
        .map(|message| message.trim().to_string())
        .filter(|message| !message.is_empty())
}

// ---------------------------------------------------------------------------
// Fields
// ---------------------------------------------------------------------------

/// The structured detail Sonarr's Slack channel has no room for.
///
/// Sonarr sends one prose sentence per event; Scryer's contract carries the
/// facts separately, so each event renders the subset that is meaningful for it.
/// Every entry is conditional on the block actually being present, which is what
/// keeps the sparse shape the core sends today from producing a wall of empty
/// labels.
fn event_fields(req: &PluginNotificationRequest) -> Vec<(&'static str, String)> {
    let mut fields: Vec<(&'static str, String)> = Vec::new();
    match req.event_type {
        NotificationEventType::Grab => {
            push(&mut fields, "Episode", episode_display(req));
            push(&mut fields, "Quality", quality(req));
            push(&mut fields, "Release", release_title(req));
            push(&mut fields, "Release Group", release_group(req));
            push(&mut fields, "Indexer", indexer(req));
            push(&mut fields, "Size", total_size_bytes(req).map(format_bytes));
            push(&mut fields, "Client", client_name(req));
        }
        // A FAILED download; see `event_label`.
        NotificationEventType::Download => {
            push(&mut fields, "Episode", episode_display(req));
            push(&mut fields, "Release", release_title(req));
            push(&mut fields, "Indexer", indexer(req));
            push(&mut fields, "Client", client_name(req));
            push(&mut fields, "Reason", download_status_message(req));
        }
        NotificationEventType::Upgrade | NotificationEventType::ImportComplete => {
            push(&mut fields, "Episode", episode_display(req));
            push(&mut fields, "Quality", quality(req));
            push(&mut fields, "Release Group", release_group(req));
            push(&mut fields, "Client", client_name(req));
            push(&mut fields, "Source", import_source_path(req));
            push(&mut fields, "Destination", import_dest_path(req));
            push(&mut fields, "Files", imported_count(req));
        }
        NotificationEventType::ImportRejected => {
            push(&mut fields, "Episode", episode_display(req));
            push(&mut fields, "Release", release_title(req));
            push(&mut fields, "Client", client_name(req));
            push(&mut fields, "Path", import_source_path(req));
        }
        NotificationEventType::Rename => {
            push(&mut fields, "Episode", episode_display(req));
            push(&mut fields, "Library Path", title_path(req));
            push(&mut fields, "Files", renamed_count(req));
        }
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            push(&mut fields, "Episode", episode_display(req));
            push(&mut fields, "Quality", quality(req));
            push(&mut fields, "File", deleted_path(req));
        }
        NotificationEventType::TitleAdded | NotificationEventType::TitleDeleted => {
            push(&mut fields, "Library Path", title_path(req));
        }
        NotificationEventType::HealthIssue | NotificationEventType::HealthRestored => {
            push(&mut fields, "Status", health_status(req));
            push(&mut fields, "Details", health_details(req));
        }
        // `Slack.OnApplicationUpdate` (`Slack.cs:174-189`) carries only the
        // message; the contract carries both versions.
        NotificationEventType::ApplicationUpdate => {
            let update = req.application_update.as_ref();
            push(
                &mut fields,
                "Previous Version",
                update.and_then(|update| update.current_version.clone()),
            );
            push(
                &mut fields,
                "New Version",
                update.and_then(|update| update.target_version.clone()),
            );
        }
        NotificationEventType::ManualInteractionRequired => {
            let interaction = req.manual_interaction.as_ref();
            push(&mut fields, "Download", download_title(req));
            push(&mut fields, "Client", client_name(req));
            push(
                &mut fields,
                "Reason",
                interaction.and_then(|interaction| interaction.reason.clone()),
            );
            push(
                &mut fields,
                "Link",
                interaction.and_then(|interaction| interaction.link.clone()),
            );
        }
        // Events Sonarr has no renderer for. Never fail on an event this channel
        // does not special-case: render what the contract carries.
        NotificationEventType::Test => {}
        _ => {
            push(&mut fields, "Episode", episode_display(req));
            push(&mut fields, "Quality", quality(req));
            push(&mut fields, "Indexer", indexer(req));
            push(&mut fields, "Client", client_name(req));
            push(&mut fields, "Path", import_dest_path(req));
        }
    }
    fields
}

fn push(fields: &mut Vec<(&'static str, String)>, label: &'static str, value: Option<String>) {
    let Some(value) = value else { return };
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    fields.push((label, value.to_string()));
}

fn episode_display(req: &PluginNotificationRequest) -> Option<String> {
    episode_detail(req)
}

fn quality(req: &PluginNotificationRequest) -> Option<String> {
    req.release
        .as_ref()
        .and_then(|release| release.quality.clone())
        .or_else(|| req.media_files.iter().find_map(|file| file.quality.clone()))
        .map(|quality| quality.trim().to_string())
        .filter(|quality| !quality.is_empty())
}

fn release_group(req: &PluginNotificationRequest) -> Option<String> {
    req.release
        .as_ref()
        .and_then(|release| release.release_group.clone())
        .or_else(|| {
            req.media_files
                .iter()
                .find_map(|file| file.release_group.clone())
        })
        .map(|group| group.trim().to_string())
        .filter(|group| !group.is_empty())
}

fn release_title(req: &PluginNotificationRequest) -> Option<String> {
    req.release
        .as_ref()
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
        .or_else(|| download_title(req))
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
}

fn download_title(req: &PluginNotificationRequest) -> Option<String> {
    req.download
        .as_ref()
        .and_then(|download| download.title.clone())
        .or_else(|| {
            req.release
                .as_ref()
                .and_then(|release| release.source_title.clone())
        })
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
}

fn indexer(req: &PluginNotificationRequest) -> Option<String> {
    req.release
        .as_ref()
        .and_then(|release| {
            release
                .indexer
                .clone()
                .or_else(|| release.source_hint.clone())
        })
        .map(|indexer| indexer.trim().to_string())
        .filter(|indexer| !indexer.is_empty())
}

fn client_name(req: &PluginNotificationRequest) -> Option<String> {
    req.download
        .as_ref()
        .and_then(|download| download.client_name.clone())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

fn download_status_message(req: &PluginNotificationRequest) -> Option<String> {
    req.download
        .as_ref()
        .and_then(|download| {
            download
                .status_message
                .clone()
                .or_else(|| download.status.clone())
        })
        .map(|status| status.trim().to_string())
        .filter(|status| !status.is_empty())
}

fn import_source_path(req: &PluginNotificationRequest) -> Option<String> {
    req.import
        .as_ref()
        .and_then(|import| import.source_path.clone())
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
}

fn import_dest_path(req: &PluginNotificationRequest) -> Option<String> {
    req.import
        .as_ref()
        .and_then(|import| import.dest_path.clone())
        .or_else(|| req.file.as_ref().and_then(|file| file.primary_path.clone()))
        .or_else(|| req.media_files.first().map(|file| file.path.clone()))
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
}

fn imported_count(req: &PluginNotificationRequest) -> Option<String> {
    req.import
        .as_ref()
        .and_then(|import| import.imported_count)
        .filter(|count| *count > 0)
        .map(|count| count.to_string())
}

fn renamed_count(req: &PluginNotificationRequest) -> Option<String> {
    let count = req
        .file
        .as_ref()
        .map(|file| file.media_updates.len())
        .unwrap_or_default();
    (count > 0).then(|| count.to_string())
}

fn title_path(req: &PluginNotificationRequest) -> Option<String> {
    req.title
        .as_ref()
        .and_then(|title| title.path.clone())
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
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
        if let Some(path) = file
            .primary_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            return Some(path.to_string());
        }
    }
    req.media_files.first().map(|file| file.path.clone())
}

fn health_status(req: &PluginNotificationRequest) -> Option<String> {
    req.health
        .as_ref()
        .and_then(|health| {
            health
                .status
                .clone()
                .or_else(|| health.severity.clone())
                .or_else(|| health.code.clone())
        })
        .map(|status| status.trim().to_string())
        .filter(|status| !status.is_empty())
}

fn health_details(req: &PluginNotificationRequest) -> Option<String> {
    req.health
        .as_ref()
        .and_then(|health| health.details.clone())
        .map(|details| details.trim().to_string())
        .filter(|details| !details.is_empty())
}

/// Sonarr prefers the release size and falls back to the sum of the imported
/// files.
fn total_size_bytes(req: &PluginNotificationRequest) -> Option<i64> {
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
// Links
// ---------------------------------------------------------------------------

/// Sonarr's Slack channel has no metadata links at all; Scryer's title carries a
/// facet and a full external-id set, so the context line offers the same links
/// the Discord and Telegram channels do — series links for an episodic facet,
/// the TMDB/IMDb pair for a movie, and the anime trackers whenever they are
/// present.
fn metadata_links(req: &PluginNotificationRequest) -> Vec<(&'static str, String)> {
    let Some(title) = req.title.as_ref() else {
        return Vec::new();
    };
    let ids = &title.external_ids;
    let facet = title.facet.to_ascii_lowercase();
    let episodic = matches!(facet.as_str(), "series" | "anime" | "tv" | "show");

    let tvdb = first_id(ids.tvdb_id.as_deref(), ids, "tvdb");
    let tmdb = first_id(ids.tmdb_id.as_deref(), ids, "tmdb");
    let imdb = first_id(ids.imdb_id.as_deref(), ids, "imdb");
    let tvmaze = first_id(ids.tvmaze_id.as_deref(), ids, "tvmaze");
    let anidb = first_id(ids.anidb_id.as_deref(), ids, "anidb");
    let anilist = first_id(ids.anilist_ids.first().map(String::as_str), ids, "anilist");
    let mal = first_id(ids.mal_ids.first().map(String::as_str), ids, "mal");
    let kitsu = first_id(ids.kitsu_ids.first().map(String::as_str), ids, "kitsu");

    let mut links: Vec<(&'static str, String)> = Vec::new();
    if episodic {
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
        if let Some(id) = &tmdb {
            links.push(("TMDB", format!("https://www.themoviedb.org/tv/{id}")));
        }
        if let Some(id) = &imdb {
            links.push(("IMDB", format!("https://imdb.com/title/{id}/")));
        }
    } else {
        if let Some(id) = &tmdb {
            links.push(("TMDB", format!("https://www.themoviedb.org/movie/{id}")));
            links.push((
                "Trakt",
                format!("https://trakt.tv/search/tmdb/{id}?id_type=movie"),
            ));
        }
        if let Some(id) = &imdb {
            links.push(("IMDB", format!("https://imdb.com/title/{id}/")));
        }
        if facet != "movie"
            && let Some(id) = &tvdb
        {
            links.push((
                "The TVDB",
                format!("https://thetvdb.com/?tab=series&id={id}"),
            ));
        }
    }

    // An anime id present is evidence enough; the facet does not have to say so.
    if let Some(id) = &anidb {
        links.push(("AniDB", format!("https://anidb.net/anime/{id}")));
    }
    if let Some(id) = &anilist {
        links.push(("AniList", format!("https://anilist.co/anime/{id}")));
    }
    if let Some(id) = &mal {
        links.push(("MyAnimeList", format!("https://myanimelist.net/anime/{id}")));
    }
    if let Some(id) = &kitsu {
        links.push(("Kitsu", format!("https://kitsu.app/anime/{id}")));
    }

    links
}

fn first_id(
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

/// Slack's link syntax is `<url|label>`
/// (<https://docs.slack.dev/messaging/formatting-message-text>), not Markdown's
/// `[label](url)`. The URL is escaped as well: an unescaped `&` inside the angle
/// brackets truncates the link.
fn links_string(req: &PluginNotificationRequest) -> Option<String> {
    let links = metadata_links(req);
    (!links.is_empty()).then(|| {
        links
            .iter()
            .map(|(label, url)| format!("<{}|{}>", mrkdwn_escape(url), mrkdwn_escape(label)))
            .collect::<Vec<_>>()
            .join(" · ")
    })
}

// ---------------------------------------------------------------------------
// Escaping and limits
// ---------------------------------------------------------------------------

/// "Slack uses `&`, `<`, and `>` as control characters for special parsing in
/// text objects, so they must be converted to HTML entities if they're not going
/// to be used for their parsing purpose"
/// (<https://docs.slack.dev/messaging/formatting-message-text>).
///
/// Only those three: the same page warns that "you shouldn't encode the entire
/// piece of text", so `"` and `'` are deliberately left alone —
/// `notify_common::html_escape` also rewrites `"`, which is why this channel
/// does not use it.
fn mrkdwn_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn ellipsize(text: &str, limit: usize) -> String {
    if char_count(text) <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
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
    if char_count(text) <= limit {
        return;
    }
    warnings.push(format!(
        "{label} truncated to Slack's {limit}-character limit"
    ));
    let clamped = ellipsize(text, limit);
    *value = json!(clamped);
}

/// Slack rejects an over-limit block outright, so trim to fit and tell the core
/// what was lost — the addendum's rule for provider limits.
fn enforce_block_limits(attachment: &mut Value, warnings: &mut Vec<String>) {
    clamp_member(
        attachment,
        "fallback",
        TEXT_LIMIT,
        "attachment fallback",
        warnings,
    );

    let Some(blocks) = attachment.get_mut("blocks").and_then(Value::as_array_mut) else {
        return;
    };

    if blocks.len() > BLOCK_COUNT_LIMIT {
        warnings.push(format!(
            "dropped {} block(s) over Slack's {BLOCK_COUNT_LIMIT}-block limit",
            blocks.len() - BLOCK_COUNT_LIMIT
        ));
        blocks.truncate(BLOCK_COUNT_LIMIT);
    }

    for block in blocks.iter_mut() {
        if let Some(text) = block.get_mut("text") {
            clamp_member(text, "text", SECTION_TEXT_LIMIT, "section text", warnings);
        }
        if let Some(fields) = block.get_mut("fields").and_then(Value::as_array_mut) {
            if fields.len() > SECTION_FIELD_COUNT_LIMIT {
                warnings.push(format!(
                    "dropped {} field(s) over Slack's {SECTION_FIELD_COUNT_LIMIT}-field limit",
                    fields.len() - SECTION_FIELD_COUNT_LIMIT
                ));
                fields.truncate(SECTION_FIELD_COUNT_LIMIT);
            }
            for entry in fields.iter_mut() {
                clamp_member(
                    entry,
                    "text",
                    SECTION_FIELD_LIMIT,
                    "section field",
                    warnings,
                );
            }
        }
        if let Some(elements) = block.get_mut("elements").and_then(Value::as_array_mut)
            && elements.len() > CONTEXT_ELEMENT_LIMIT
        {
            warnings.push(format!(
                "dropped {} context element(s) over Slack's {CONTEXT_ELEMENT_LIMIT}-element limit",
                elements.len() - CONTEXT_ELEMENT_LIMIT
            ));
            elements.truncate(CONTEXT_ELEMENT_LIMIT);
        }
    }
}

fn enforce_limits(payload: &mut Value, warnings: &mut Vec<String>) {
    clamp_member(payload, "text", TEXT_LIMIT, "message text", warnings);
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

fn send_notification(req: &PluginNotificationRequest) -> PluginResult<PluginNotificationResponse> {
    let settings = match Settings::from_config(req.is_test) {
        Ok(settings) => settings,
        Err(error) => return PluginResult::Err(error),
    };

    let (payload, warnings) = build_payload(req, &settings);
    let body = match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(error) => {
            return PluginResult::Err(plugin_error(
                PluginErrorCode::Permanent,
                "could not encode the Slack webhook payload".to_string(),
                Some(error.to_string()),
            ));
        }
    };

    let request = HttpRequest::new(&settings.webhook_url)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);

    match http::request::<Vec<u8>>(&request, Some(body)) {
        Ok(response) => classify_response(
            response.status_code(),
            response.headers(),
            &response.body(),
            warnings,
        ),
        Err(error) => {
            // The host answers a refused or failed egress in-band; report it as
            // a delivery failure rather than a channel misconfiguration.
            let mut failure = error_response(format!("request failed: {error}"), None);
            failure.warnings = warnings;
            PluginResult::Ok(failure)
        }
    }
}

/// Slack's incoming-webhook failures, mapped to Scryer's two error lanes.
///
/// The **body** is authoritative and the status is the fallback: Slack answers
/// with a plain-text error string
/// (<https://docs.slack.dev/messaging/sending-messages-using-incoming-webhooks>)
/// and the status only bands it (400 / 403 / 404 / 410 / 500, added by
/// <https://docs.slack.dev/changelog/2016-05-17-changes-to-errors-for-incoming-webhooks/>).
/// Reading the string first means `user_not_found` — a 400 that is really the
/// `channel` setting — lands on the operator's setting rather than on "the
/// plugin built a bad payload".
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    warnings: Vec<String>,
) -> PluginResult<PluginNotificationResponse> {
    let detail = slack_detail(body);

    if (200..300).contains(&status) {
        let mut response = ok_response();
        response.warnings = warnings;
        return PluginResult::Ok(response);
    }

    if status == 429 {
        let mut failure = error_response(
            format!("Slack rate-limited this webhook (HTTP 429): {detail}"),
            Some("http_429".to_string()),
        );
        failure.retry_after_seconds = retry_after_seconds(headers);
        failure.warnings = warnings;
        return PluginResult::Ok(failure);
    }

    let code = detail.trim().to_ascii_lowercase();
    match code.as_str() {
        // The webhook itself is gone or was never valid: the operator has to
        // paste a new URL. Retrying changes nothing.
        "no_service" | "no_service_id" | "no_active_hooks" | "invalid_token" | "no_team"
        | "team_disabled" => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!(
                "webhook_url was rejected by Slack (HTTP {status}: {code}). Recreate the incoming webhook and paste the new URL."
            ),
            Some(format!("HTTP {status}: {detail}")),
        )),
        // The destination the operator named does not accept this message.
        "channel_not_found"
        | "channel_is_archived"
        | "user_not_found"
        | "posting_to_general_channel_denied" => PluginResult::Err(plugin_error(
            PluginErrorCode::InvalidConfig,
            format!("channel was rejected by Slack (HTTP {status}: {code})"),
            Some(format!("HTTP {status}: {detail}")),
        )),
        // A workspace-level restriction on this webhook: not a payload problem
        // and not something a new URL fixes.
        "action_prohibited" => PluginResult::Err(plugin_error(
            PluginErrorCode::AuthFailed,
            format!("Slack refused this posting method (HTTP {status}: {code})"),
            Some(format!("HTTP {status}: {detail}")),
        )),
        // The payload this plugin built is wrong; retrying it changes nothing.
        "invalid_payload" | "no_text" | "too_many_attachments" => PluginResult::Err(plugin_error(
            PluginErrorCode::Permanent,
            format!("Slack rejected the notification payload (HTTP {status}: {code})"),
            Some(format!("HTTP {status}: {detail}")),
        )),
        _ => match status {
            400 => PluginResult::Err(plugin_error(
                PluginErrorCode::Permanent,
                format!("Slack rejected the notification payload (HTTP 400): {detail}"),
                Some(format!("HTTP 400: {detail}")),
            )),
            401 | 403 => PluginResult::Err(plugin_error(
                PluginErrorCode::AuthFailed,
                format!("Slack refused this webhook (HTTP {status}): {detail}"),
                Some(format!("HTTP {status}: {detail}")),
            )),
            404 | 410 => PluginResult::Err(plugin_error(
                PluginErrorCode::InvalidConfig,
                format!(
                    "webhook_url is no longer usable (HTTP {status}): {detail}. Recreate the incoming webhook and paste the new URL."
                ),
                Some(format!("HTTP {status}: {detail}")),
            )),
            // Everything else — 5xx included — is the provider saying no for
            // now. It stays on the delivery-result lane so the operator sees
            // Slack's own answer, and so an outage is never reported as a
            // broken channel. `tests/host_conformance.rs` pins this for 500.
            _ => {
                let mut failure = error_response(
                    format!("HTTP {status}: {detail}"),
                    Some(format!("http_{status}")),
                );
                failure.warnings = warnings;
                PluginResult::Ok(failure)
            }
        },
    }
}

/// Slack's webhook error body is a plain-text string ("in most cases you'll
/// receive a `HTTP 200` response with a plain text `ok`"), but a Slack-compatible
/// endpoint may answer with the Web API's JSON `{"ok":false,"error":"…"}`
/// instead, so both are read.
fn slack_detail(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text)
        && let Some(error) = map
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|error| !error.is_empty())
    {
        return error.to_string();
    }
    if text.is_empty() {
        "no response body".to_string()
    } else {
        ellipsize(text, 500)
    }
}

/// "you'll receive a HTTP 429 Too Many Requests error, and a `Retry-After` HTTP
/// header containing the number of seconds until you can retry"
/// (<https://docs.slack.dev/apis/web-api/rate-limits/>). Slack sends whole
/// seconds; a fractional value from a compatible endpoint is rounded up.
fn retry_after_seconds(headers: &BTreeMap<String, String>) -> Option<i64> {
    let seconds = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<f64>().ok())?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((seconds.ceil() as i64).max(1))
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
        PluginNotificationDownload, PluginNotificationExternalIds, PluginNotificationFile,
        PluginNotificationHealth, PluginNotificationImport, PluginNotificationManualInteraction,
        PluginNotificationMediaUpdate, PluginNotificationRelease, PluginNotificationTitle,
    };

    fn request(event_type: NotificationEventType) -> PluginNotificationRequest {
        PluginNotificationRequest {
            schema_version: 1,
            event_type,
            event_id: None,
            occurred_at: None,
            correlation_id: None,
            actor: None,
            severity: None,
            is_test: event_type == NotificationEventType::Test,
            summary_title: "Summary".to_string(),
            summary_message: "Summary message.".to_string(),
            app: PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "0.0.0-test".to_string(),
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
            name: "Cinder Line".to_string(),
            facet: "series".to_string(),
            year: Some(2019),
            slug: None,
            path: Some("/media/TV/Cinder Line".to_string()),
            overview: Some("A show about a line.".to_string()),
            sort_title: None,
            background_url: Some("https://images.test/fanart.jpg".to_string()),
            poster_url: Some("https://images.test/poster.jpg".to_string()),
            tags: Vec::new(),
            aliases: Vec::new(),
            original_language: None,
            original_country: None,
            external_ids: PluginNotificationExternalIds {
                tvdb_id: Some("12345".to_string()),
                imdb_id: Some("tt0999".to_string()),
                ..Default::default()
            },
        }
    }

    fn episode(season: &str, number: &str, title: &str) -> PluginNotificationEpisode {
        PluginNotificationEpisode {
            season_number: Some(season.to_string()),
            episode_number: Some(number.to_string()),
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    /// Everything the contract can carry, so every renderer has data.
    fn fully_populated_grab() -> PluginNotificationRequest {
        let mut req = request(NotificationEventType::Grab);
        req.occurred_at = Some("2026-09-01T10:00:00Z".to_string());
        req.summary_title = "Grabbed: Cinder Line".to_string();
        req.summary_message = "Grabbed 'Cinder.Line.S02E03' for 'Cinder Line'.".to_string();
        req.title = Some(series_title());
        req.episode = Some(episode("2", "3", "Trackside"));
        req.episodes = vec![episode("2", "3", "Trackside")];
        req.release = Some(PluginNotificationRelease {
            source_title: Some("Cinder.Line.S02E03.1080p.WEB-DL".to_string()),
            quality: Some("WEBDL-1080p".to_string()),
            release_group: Some("SCRY".to_string()),
            indexer: Some("NZBGeek".to_string()),
            ..Default::default()
        });
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            size_bytes: Some(2_147_483_648),
            title: Some("Cinder.Line.S02E03.1080p.WEB-DL".to_string()),
            ..Default::default()
        });
        req
    }

    /// The shape the core actually sends today for a grab: title, episode ids,
    /// release source title, download id — and nothing else.
    fn sparse_grab() -> PluginNotificationRequest {
        let mut req = request(NotificationEventType::Grab);
        req.occurred_at = Some("2026-09-01T10:00:00Z".to_string());
        req.summary_title = "Grabbed: Cinder Line".to_string();
        req.summary_message = "Grabbed 'Cinder.Line.S02E03'.".to_string();
        req.title = Some(series_title());
        req.episode = Some(PluginNotificationEpisode {
            episode_ids: vec!["ep-1".to_string()],
            ..Default::default()
        });
        req.release = Some(PluginNotificationRelease {
            source_title: Some("Cinder.Line.S02E03.1080p.WEB-DL".to_string()),
            ..Default::default()
        });
        req
    }

    fn render(req: &PluginNotificationRequest) -> (Value, Vec<String>) {
        build_payload(req, &Settings::defaults())
    }

    fn attachment_of(payload: &Value) -> &Value {
        &payload["attachments"][0]
    }

    fn blocks_of(payload: &Value) -> &Vec<Value> {
        attachment_of(payload)["blocks"].as_array().unwrap()
    }

    fn section_text(payload: &Value) -> &str {
        blocks_of(payload)[0]["text"]["text"].as_str().unwrap()
    }

    fn field_texts(payload: &Value) -> Vec<String> {
        blocks_of(payload)
            .iter()
            .filter_map(|block| block.get("fields"))
            .flat_map(|fields| fields.as_array().unwrap().iter())
            .map(|field| field["text"].as_str().unwrap().to_string())
            .collect()
    }

    fn context_texts(payload: &Value) -> Vec<String> {
        blocks_of(payload)
            .iter()
            .filter(|block| block["type"] == "context")
            .flat_map(|block| block["elements"].as_array().unwrap().iter())
            .map(|element| element["text"].as_str().unwrap().to_string())
            .collect()
    }

    // -- descriptor ---------------------------------------------------------

    #[test]
    fn descriptor_keeps_sonarrs_settings_and_declares_the_event_filters() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Notification(notification) = descriptor.provider else {
            panic!("expected a notification provider");
        };
        assert_eq!(notification.provider_type, "slack");
        assert!(
            notification
                .capabilities
                .event_options
                .supports_upgrade_filter
        );
        assert!(
            notification
                .capabilities
                .event_options
                .supports_delete_for_upgrade_filter
        );
        assert!(
            notification
                .capabilities
                .event_options
                .supports_health_warning_filter
        );
        assert!(
            !notification.capabilities.supports_images,
            "Slack fetches images server-side; this channel sends none"
        );

        // Existing keys are a public contract and must not be renamed.
        for key in ["webhook_url", "username", "icon", "channel"] {
            assert!(
                notification.config_fields.iter().any(|f| f.key == key),
                "{key} is missing"
            );
        }
        let username = notification
            .config_fields
            .iter()
            .find(|f| f.key == "username")
            .unwrap();
        assert!(username.required, "Sonarr requires Username");
        assert_eq!(username.default_value.as_deref(), Some("Scryer"));
    }

    // -- settings validation ------------------------------------------------

    #[test]
    fn webhook_url_must_be_an_http_url() {
        assert!(validate_webhook_url("https://hooks.slack.com/services/T/B/x").is_ok());
        assert!(validate_webhook_url("http://mattermost.lan/hooks/abc").is_ok());
        let error = validate_webhook_url("hooks.slack.com/services/T/B/x").unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("webhook_url"));
    }

    #[test]
    fn username_defaults_when_unresolved_and_is_rejected_when_blanked() {
        assert_eq!(validated_username(None).unwrap(), "Scryer");
        assert_eq!(
            validated_username(Some(" Media Bot ")).unwrap(),
            "Media Bot"
        );
        let error = validated_username(Some("   ")).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("username"));
    }

    #[test]
    fn icon_follows_sonarrs_emoji_versus_url_rule() {
        assert_eq!(
            validated_icon(Some(":robot_face:"), true).unwrap().0,
            Some(Icon::Emoji(":robot_face:".to_string()))
        );
        assert_eq!(
            validated_icon(Some("https://img.test/a.png"), true)
                .unwrap()
                .0,
            Some(Icon::Url("https://img.test/a.png".to_string()))
        );
        assert_eq!(validated_icon(None, true).unwrap(), (None, None));
    }

    #[test]
    fn an_uninterpretable_icon_fails_the_test_and_degrades_a_live_send() {
        let error = validated_icon(Some("robot_face"), true).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("icon"));

        let (icon, warning) = validated_icon(Some("robot_face"), false).unwrap();
        assert!(icon.is_none(), "a live send drops it rather than failing");
        assert!(warning.unwrap().contains("robot_face"));
    }

    // -- identity fields ----------------------------------------------------

    #[test]
    fn identity_fields_follow_sonarrs_create_payload() {
        let settings = Settings {
            username: "Media Bot".to_string(),
            icon: Some(Icon::Emoji(":tv:".to_string())),
            channel: Some("#media".to_string()),
            ..Settings::defaults()
        };
        let (payload, _) = build_payload(&fully_populated_grab(), &settings);
        assert_eq!(payload["username"], "Media Bot");
        assert_eq!(payload["icon_emoji"], ":tv:");
        assert!(payload.get("icon_url").is_none());
        assert_eq!(payload["channel"], "#media");

        let settings = Settings {
            icon: Some(Icon::Url("https://img.test/a.png".to_string())),
            ..Settings::defaults()
        };
        let (payload, _) = build_payload(&fully_populated_grab(), &settings);
        assert_eq!(payload["icon_url"], "https://img.test/a.png");
        assert!(payload.get("icon_emoji").is_none());
        assert!(payload.get("channel").is_none());
    }

    #[test]
    fn an_icon_warning_reaches_the_response() {
        let settings = Settings {
            icon_warning: Some("icon \"robot_face\" was not sent".to_string()),
            ..Settings::defaults()
        };
        let (_, warnings) = build_payload(&fully_populated_grab(), &settings);
        assert!(warnings.iter().any(|w| w.contains("robot_face")));
    }

    // -- test message (M2) --------------------------------------------------

    #[test]
    fn a_test_is_plain_text_with_a_timestamp_and_no_attachment() {
        let mut req = request(NotificationEventType::Test);
        req.occurred_at = Some("2026-09-01T10:00:00Z".to_string());
        let (payload, warnings) = render(&req);
        assert_eq!(
            payload["text"],
            "Test message from Scryer posted at 2026-09-01T10:00:00Z"
        );
        assert!(
            payload.get("attachments").is_none(),
            "Sonarr's test carries no attachment"
        );
        assert_eq!(payload["username"], "Scryer");
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_test_without_a_timestamp_still_renders() {
        let (payload, _) = render(&request(NotificationEventType::Test));
        assert_eq!(payload["text"], "Test message from Scryer");
    }

    // -- headings (M1) ------------------------------------------------------

    #[test]
    fn the_heading_is_the_title_plus_the_episode_detail() {
        let mut req = fully_populated_grab();
        req.episode = Some(PluginNotificationEpisode {
            display: Some("S02E03 - Trackside".to_string()),
            ..Default::default()
        });
        assert_eq!(heading(&req), "Cinder Line - S02E03 - Trackside");
    }

    #[test]
    fn the_heading_composes_sonarrs_multi_episode_shape_without_a_display() {
        let mut req = fully_populated_grab();
        req.episode = None;
        req.episodes = vec![episode("2", "3", "Trackside"), episode("2", "4", "Sidings")];
        assert_eq!(heading(&req), "Cinder Line - 2x03x04 - Trackside + Sidings");
    }

    #[test]
    fn the_heading_uses_sonarrs_daily_shape_when_there_is_no_episode_number() {
        let mut req = fully_populated_grab();
        req.episode = None;
        req.episodes = vec![PluginNotificationEpisode {
            air_date: Some("2026-08-30".to_string()),
            title: Some("Friday".to_string()),
            ..Default::default()
        }];
        assert_eq!(heading(&req), "Cinder Line - 2026-08-30 - Friday");
    }

    #[test]
    fn the_heading_is_the_bare_title_when_no_episode_is_carried() {
        let mut req = request(NotificationEventType::TitleAdded);
        req.title = Some(series_title());
        assert_eq!(heading(&req), "Cinder Line");
    }

    #[test]
    fn a_health_heading_is_the_health_source_not_the_title() {
        let mut req = request(NotificationEventType::HealthIssue);
        req.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable due to failures".to_string()),
            ..Default::default()
        });
        assert_eq!(heading(&req), "IndexerStatusCheck");
        assert_eq!(
            attachment_body(&req),
            "Indexers unavailable due to failures"
        );
    }

    #[test]
    fn health_restored_keeps_sonarrs_resolved_sentence() {
        let mut req = request(NotificationEventType::HealthRestored);
        req.health = Some(PluginNotificationHealth {
            code: Some("IndexerStatusCheck".to_string()),
            message: Some("Indexers unavailable".to_string()),
            ..Default::default()
        });
        assert_eq!(
            attachment_body(&req),
            "The following issue is now resolved: Indexers unavailable"
        );
    }

    #[test]
    fn application_update_heads_with_the_app_name_because_no_machine_name_exists() {
        let mut req = request(NotificationEventType::ApplicationUpdate);
        req.application_update = Some(PluginNotificationApplicationUpdate {
            current_version: Some("0.19.7".to_string()),
            target_version: Some("0.19.8".to_string()),
            summary: Some("Updated to 0.19.8".to_string()),
            ..Default::default()
        });
        assert_eq!(heading(&req), "Scryer");
        assert_eq!(attachment_body(&req), "Updated to 0.19.8");
        let (payload, _) = render(&req);
        let fields = field_texts(&payload);
        assert!(fields.iter().any(|f| f.contains("Previous Version")));
        assert!(fields.iter().any(|f| f.ends_with("0.19.8")));
    }

    #[test]
    fn a_long_heading_is_ellipsized() {
        let mut req = fully_populated_grab();
        req.episode = None;
        req.episodes = Vec::new();
        req.title = Some(PluginNotificationTitle {
            name: "N".repeat(400),
            ..series_title()
        });
        let heading = heading(&req);
        assert_eq!(char_count(&heading), HEADING_LIMIT);
        assert!(heading.ends_with('…'));
    }

    // -- the Download event is a failure ------------------------------------

    #[test]
    fn download_renders_a_failure_never_an_import() {
        let mut req = request(NotificationEventType::Download);
        req.severity = Some(NotificationSeverity::Error);
        req.summary_title = "Download failed: Cinder Line".to_string();
        req.summary_message = "The download failed after 3 attempts.".to_string();
        req.title = Some(series_title());
        req.download = Some(PluginNotificationDownload {
            client_name: Some("Weaver".to_string()),
            status: Some("failed".to_string()),
            status_message: Some("unpack error".to_string()),
            ..Default::default()
        });

        assert_eq!(event_label(&req), "Download Failed");
        let (payload, _) = render(&req);
        assert_eq!(payload["text"], "Download failed: Cinder Line");
        assert!(
            !section_text(&payload).contains("Imported"),
            "the June port said \"Imported:\" for this event"
        );
        assert_eq!(attachment_of(&payload)["color"], COLOR_DANGER);
        assert!(
            field_texts(&payload)
                .iter()
                .any(|f| f.contains("Reason") && f.contains("unpack error"))
        );
    }

    // -- colours ------------------------------------------------------------

    #[test]
    fn colours_follow_sonarrs_table_over_scryers_event_enum() {
        for (event, expected) in [
            (NotificationEventType::Grab, COLOR_WARNING),
            (NotificationEventType::HealthIssue, COLOR_WARNING),
            (
                NotificationEventType::ManualInteractionRequired,
                COLOR_WARNING,
            ),
            (NotificationEventType::ImportComplete, COLOR_GOOD),
            (NotificationEventType::Upgrade, COLOR_GOOD),
            (NotificationEventType::HealthRestored, COLOR_GOOD),
            (NotificationEventType::TitleAdded, COLOR_GOOD),
            (NotificationEventType::FileDeleted, COLOR_DANGER),
            (NotificationEventType::TitleDeleted, COLOR_DANGER),
            (NotificationEventType::Download, COLOR_DANGER),
            (NotificationEventType::Rename, COLOR_INFO),
        ] {
            assert_eq!(
                attachment_color(&request(event)),
                expected,
                "wrong colour for {event:?}"
            );
        }
    }

    #[test]
    fn severity_overrides_the_event_colour_but_never_downgrades_danger() {
        let mut req = request(NotificationEventType::TitleAdded);
        req.severity = Some(NotificationSeverity::Error);
        assert_eq!(attachment_color(&req), COLOR_DANGER);

        req.severity = Some(NotificationSeverity::Warning);
        assert_eq!(attachment_color(&req), COLOR_WARNING);

        let mut deleted = request(NotificationEventType::FileDeleted);
        deleted.severity = Some(NotificationSeverity::Warning);
        assert_eq!(attachment_color(&deleted), COLOR_DANGER);
    }

    // -- fields -------------------------------------------------------------

    #[test]
    fn a_grab_renders_the_structured_fields_the_contract_carries() {
        let (payload, warnings) = render(&fully_populated_grab());
        let fields = field_texts(&payload);
        assert!(fields.iter().any(|f| f == "*Episode*\n2x03 - Trackside"));
        assert!(fields.iter().any(|f| f == "*Quality*\nWEBDL-1080p"));
        assert!(fields.iter().any(|f| f == "*Indexer*\nNZBGeek"));
        assert!(fields.iter().any(|f| f == "*Size*\n2 GB"));
        assert!(fields.iter().any(|f| f == "*Client*\nWeaver"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn the_sparse_shape_the_core_sends_today_renders_without_empty_labels() {
        let (payload, warnings) = render(&sparse_grab());
        assert_eq!(payload["text"], "Grabbed: Cinder Line");
        assert_eq!(
            section_text(&payload),
            "*Cinder Line*\nGrabbed 'Cinder.Line.S02E03'."
        );
        let fields = field_texts(&payload);
        assert_eq!(fields, vec!["*Release*\nCinder.Line.S02E03.1080p.WEB-DL"]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_file_delete_names_the_deleted_path() {
        let mut req = request(NotificationEventType::FileDeleted);
        req.title = Some(series_title());
        req.summary_message = "Deleted for upgrade".to_string();
        req.file = Some(PluginNotificationFile {
            primary_path: None,
            media_updates: vec![PluginNotificationMediaUpdate {
                path: "/media/TV/Cinder Line/S02E03.mkv".to_string(),
                update_type: NotificationMediaUpdateType::Deleted,
            }],
        });
        let (payload, _) = render(&req);
        assert!(
            field_texts(&payload)
                .iter()
                .any(|f| f == "*File*\n/media/TV/Cinder Line/S02E03.mkv")
        );
    }

    #[test]
    fn an_import_names_its_source_and_destination() {
        let mut req = request(NotificationEventType::ImportComplete);
        req.title = Some(series_title());
        req.import = Some(PluginNotificationImport {
            source_path: Some("/downloads/complete/x".to_string()),
            dest_path: Some("/media/TV/Cinder Line/S02E03.mkv".to_string()),
            imported_count: Some(2),
            ..Default::default()
        });
        let (payload, _) = render(&req);
        let fields = field_texts(&payload);
        assert!(
            fields
                .iter()
                .any(|f| f == "*Source*\n/downloads/complete/x")
        );
        assert!(
            fields
                .iter()
                .any(|f| f == "*Destination*\n/media/TV/Cinder Line/S02E03.mkv")
        );
        assert!(fields.iter().any(|f| f == "*Files*\n2"));
    }

    #[test]
    fn manual_interaction_renders_the_contract_block() {
        let mut req = request(NotificationEventType::ManualInteractionRequired);
        req.download = Some(PluginNotificationDownload {
            title: Some("Cinder.Line.S02E03".to_string()),
            client_name: Some("Weaver".to_string()),
            ..Default::default()
        });
        req.manual_interaction = Some(PluginNotificationManualInteraction {
            reason: Some("Password required".to_string()),
            link: Some("https://scryer.lan/queue".to_string()),
            ..Default::default()
        });
        let (payload, _) = render(&req);
        let fields = field_texts(&payload);
        assert!(fields.iter().any(|f| f == "*Download*\nCinder.Line.S02E03"));
        assert!(fields.iter().any(|f| f == "*Reason*\nPassword required"));
    }

    #[test]
    fn an_event_this_channel_does_not_special_case_still_renders() {
        let mut req = request(NotificationEventType::SubtitleDownloaded);
        req.title = Some(series_title());
        req.release = Some(PluginNotificationRelease {
            quality: Some("WEBDL-1080p".to_string()),
            ..Default::default()
        });
        let (payload, _) = render(&req);
        assert_eq!(attachment_of(&payload)["color"], COLOR_GOOD);
        assert!(
            field_texts(&payload)
                .iter()
                .any(|f| f == "*Quality*\nWEBDL-1080p")
        );
    }

    // -- neutral wording for a non-series facet ------------------------------

    #[test]
    fn labels_drop_sonarrs_episode_wording_for_a_non_episodic_facet() {
        let mut req = request(NotificationEventType::TitleAdded);
        req.title = Some(PluginNotificationTitle {
            facet: "movie".to_string(),
            ..series_title()
        });
        assert_eq!(event_label(&req), "Added");

        req.title = Some(series_title());
        assert_eq!(event_label(&req), "Series Added");
    }

    // -- links ---------------------------------------------------------------

    #[test]
    fn links_use_slacks_angle_bracket_syntax_with_escaped_urls() {
        let (payload, _) = render(&fully_populated_grab());
        let context = context_texts(&payload);
        assert!(
            context[0].contains("<https://thetvdb.com/?tab=series&amp;id=12345|The TVDB>"),
            "an unescaped & truncates a Slack link: {context:?}"
        );
        assert!(context[0].contains("<https://imdb.com/title/tt0999/|IMDB>"));
        assert!(context.last().unwrap().starts_with("Scryer · "));
    }

    #[test]
    fn a_movie_facet_gets_the_movie_links() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(PluginNotificationTitle {
            facet: "movie".to_string(),
            external_ids: PluginNotificationExternalIds {
                tmdb_id: Some("603".to_string()),
                ..Default::default()
            },
            ..series_title()
        });
        let links = metadata_links(&req);
        assert_eq!(links[0].0, "TMDB");
        assert!(links[0].1.contains("/movie/603"));
        assert!(links.iter().all(|(label, _)| *label != "TVmaze"));
    }

    #[test]
    fn a_title_without_ids_renders_no_link_element() {
        let mut req = request(NotificationEventType::Grab);
        req.title = Some(PluginNotificationTitle {
            external_ids: PluginNotificationExternalIds::default(),
            ..series_title()
        });
        let (payload, _) = render(&req);
        assert_eq!(context_texts(&payload).len(), 1, "only the provenance line");
    }

    // -- escaping and limits -------------------------------------------------

    #[test]
    fn only_slacks_three_control_characters_are_escaped() {
        assert_eq!(
            mrkdwn_escape("Tom & Jerry <b> \"quoted\" 'x'"),
            "Tom &amp; Jerry &lt;b&gt; \"quoted\" 'x'"
        );
    }

    #[test]
    fn the_rendered_payload_escapes_control_characters_in_every_text_object() {
        let mut req = fully_populated_grab();
        req.summary_title = "Grabbed: Tom & Jerry".to_string();
        req.summary_message = "<script>alert(1)</script>".to_string();
        req.title = Some(PluginNotificationTitle {
            name: "Tom & Jerry".to_string(),
            ..series_title()
        });
        let (payload, _) = render(&req);
        assert_eq!(payload["text"], "Grabbed: Tom &amp; Jerry");
        assert!(section_text(&payload).starts_with("*Tom &amp; Jerry"));
        assert!(section_text(&payload).contains("&lt;script&gt;"));
        assert!(
            attachment_of(&payload)["fallback"]
                .as_str()
                .unwrap()
                .contains("Tom &amp; Jerry")
        );
    }

    #[test]
    fn an_over_long_message_text_is_trimmed_with_a_warning() {
        let mut req = fully_populated_grab();
        req.summary_title = "G".repeat(TEXT_LIMIT + 500);
        let (payload, warnings) = render(&req);
        assert_eq!(char_count(payload["text"].as_str().unwrap()), TEXT_LIMIT);
        assert!(warnings.iter().any(|w| w.contains("message text")));
    }

    #[test]
    fn an_over_long_section_body_is_trimmed_with_a_warning() {
        let mut req = fully_populated_grab();
        req.summary_message = "M".repeat(SECTION_TEXT_LIMIT + 500);
        let (payload, warnings) = render(&req);
        assert_eq!(char_count(section_text(&payload)), SECTION_TEXT_LIMIT);
        assert!(warnings.iter().any(|w| w.contains("section text")));
    }

    #[test]
    fn no_event_renders_more_than_slacks_ten_section_fields() {
        for event in [
            NotificationEventType::Grab,
            NotificationEventType::Download,
            NotificationEventType::Upgrade,
            NotificationEventType::ImportComplete,
            NotificationEventType::ManualInteractionRequired,
        ] {
            let mut req = fully_populated_grab();
            req.event_type = event;
            let (payload, warnings) = render(&req);
            let count = blocks_of(&payload)
                .iter()
                .filter_map(|block| block.get("fields"))
                .map(|fields| fields.as_array().unwrap().len())
                .sum::<usize>();
            assert!(
                count <= SECTION_FIELD_COUNT_LIMIT,
                "{event:?} rendered {count}"
            );
            assert!(
                !warnings.iter().any(|w| w.contains("field(s) over")),
                "{event:?} should not need trimming"
            );
        }
    }

    #[test]
    fn format_bytes_matches_sonarrs_scale() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(2_147_483_648), "2 GB");
    }

    // -- delivery classification (H1) ----------------------------------------

    fn classify(status: u16, body: &str) -> PluginResult<PluginNotificationResponse> {
        classify_response(status, &BTreeMap::new(), body.as_bytes(), Vec::new())
    }

    fn error_of(result: PluginResult<PluginNotificationResponse>) -> PluginError {
        match result {
            PluginResult::Err(error) => error,
            other => panic!("expected a typed plugin error, got {other:?}"),
        }
    }

    fn response_of(result: PluginResult<PluginNotificationResponse>) -> PluginNotificationResponse {
        match result {
            PluginResult::Ok(response) => response,
            other => panic!("expected a delivery result, got {other:?}"),
        }
    }

    #[test]
    fn a_plain_ok_is_a_successful_delivery() {
        let response = response_of(classify(200, "ok"));
        assert!(response.success);
    }

    #[test]
    fn a_dead_webhook_names_the_webhook_url_setting() {
        for (status, body) in [
            (404, "no_service"),
            (403, "invalid_token"),
            (404, "no_team"),
        ] {
            let error = error_of(classify(status, body));
            assert_eq!(error.code, PluginErrorCode::InvalidConfig, "{body}");
            assert!(
                error.public_message.contains("webhook_url"),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn a_bad_destination_names_the_channel_setting() {
        for (status, body) in [
            (404, "channel_not_found"),
            (410, "channel_is_archived"),
            (400, "user_not_found"),
        ] {
            let error = error_of(classify(status, body));
            assert_eq!(error.code, PluginErrorCode::InvalidConfig, "{body}");
            assert!(
                error.public_message.contains("channel"),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn a_workspace_restriction_is_an_auth_failure() {
        let error = error_of(classify(403, "action_prohibited"));
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
    }

    #[test]
    fn a_payload_slack_cannot_read_is_permanent() {
        for body in ["invalid_payload", "no_text", "too_many_attachments"] {
            let error = error_of(classify(400, body));
            assert_eq!(error.code, PluginErrorCode::Permanent, "{body}");
            assert!(error.debug_message.unwrap().contains(body));
        }
    }

    #[test]
    fn a_429_reports_retry_after_from_the_header() {
        let headers = BTreeMap::from([("Retry-After".to_string(), "30".to_string())]);
        let response = response_of(classify_response(
            429,
            &headers,
            b"rate_limited",
            Vec::new(),
        ));
        assert!(!response.success);
        assert_eq!(response.retry_after_seconds, Some(30));
        assert_eq!(response.provider_status.as_deref(), Some("http_429"));
    }

    #[test]
    fn a_429_without_a_header_is_still_a_delivery_failure() {
        let response = response_of(classify(429, ""));
        assert!(!response.success);
        assert_eq!(response.retry_after_seconds, None);
    }

    /// `tests/host_conformance.rs` pins this: a provider outage stays on the
    /// delivery-result lane so it is never reported as a broken channel.
    #[test]
    fn a_5xx_stays_a_reported_delivery_failure() {
        let response = response_of(classify(500, "rollup_error"));
        assert!(!response.success);
        assert_eq!(response.provider_status.as_deref(), Some("http_500"));
        assert!(response.error.unwrap().contains("rollup_error"));
    }

    #[test]
    fn a_json_error_body_from_a_compatible_endpoint_is_read_too() {
        let error = error_of(classify(404, r#"{"ok":false,"error":"channel_not_found"}"#));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("channel"));
    }

    #[test]
    fn an_unknown_error_string_falls_back_to_the_status_band() {
        let error = error_of(classify(400, "something_new"));
        assert_eq!(error.code, PluginErrorCode::Permanent);
        let error = error_of(classify(410, "something_new"));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn an_empty_body_still_produces_a_readable_detail() {
        assert_eq!(slack_detail(b""), "no response body");
        assert_eq!(slack_detail(b"  ok  "), "ok");
    }
}
